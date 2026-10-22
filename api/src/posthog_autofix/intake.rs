//! One tick of the intake job: watchdog, discover, settle and judge, check
//! for regressions, then dispatch (live) or preview (dry run).

use super::config::{Config, Mode};
use super::emails;
use super::github::{Dispatch, DispatchError, Dispatcher};
use super::ledger::{self, Judgement, Verdict};
use super::model::{RunView, SkipReason, Source};
use super::notify::{self, Email, Mailer};
use super::policy::{self, Candidate};
use super::posthog::{Issue, IssueEvents, IssueSource};
use super::signature::{self, Fingerprint};
use super::writeback::IssueWriter;
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{MySqlConnection, MySqlPool};
use std::collections::HashMap;
use std::fmt::Display;
use utoipa::ToSchema;

/// Everything a tick talks to.
pub struct Deps<'a, S, D, M, W> {
    pub pool: &'a MySqlPool,
    pub config: &'a Config,
    pub source: &'a S,
    pub dispatcher: &'a D,
    pub mailer: &'a M,
    pub writer: &'a W,
}

/// What a tick did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TickReport {
    /// Ledger rows this tick changed (the time-triggered Lambda logs it).
    pub processed: usize,
    /// New issues recorded.
    pub discovered: usize,
    /// Issues judged eligible and queued.
    pub queued: usize,
    /// Issues skipped as noise, resolved, suppressed or not production.
    pub skipped: usize,
    /// Issues folded into an earlier run of the same bug.
    pub duplicates: usize,
    /// Fix runs started.
    pub dispatched: usize,
    /// Runs a dry run would have started (emailed instead).
    pub previewed: usize,
    /// Runs given up by the watchdog.
    pub timed_out: usize,
    /// Fixed issues that PostHog reopened (emailed).
    pub regressions: usize,
    /// PostHog issues of opened fixes assigned to their reviewer (write-back).
    pub assigned: usize,
    /// A storm digest was emailed.
    pub storm_digest: bool,
    /// Another tick held the lock; this one did nothing.
    pub busy: bool,
    /// Steps that failed (PostHog or GitHub unreachable); retried next tick.
    pub errors: Vec<String>,
}

/// Issues read from PostHog per poll.
const DISCOVER_LIMIT: u32 = 200;
/// Discovery re-reads this far behind the newest recorded issue, for ties
/// and rows PostHog made visible late.
const DISCOVER_OVERLAP_MINUTES: i64 = 10;
/// Settled issues judged per poll.
const JUDGE_BATCH: u32 = 20;
/// Events are read from this long before the oldest issue of a batch.
const EVENTS_LOOKBACK_HOURS: i64 = 1;
const STORM_TOP: u32 = 5;
/// A fix is watched for a regression this long after it finished.
const REGRESSION_DAYS: i64 = 30;
const REGRESSION_BATCH: u32 = 100;
/// An opened fix's issue is assigned within this many days, or not at all.
const ASSIGN_DAYS: i64 = 7;
const ASSIGN_BATCH: u32 = 20;
const PREVIEW_BATCH: u32 = 20;

// Per database: dev and production share one MySQL server, and so do tests.
const LOCK: &str = "SELECT GET_LOCK(CONCAT('posthog_autofix_tick:', DATABASE()), 0)";
const UNLOCK: &str = "SELECT RELEASE_LOCK(CONCAT('posthog_autofix_tick:', DATABASE()))";

pub async fn tick<S: IssueSource, D: Dispatcher, M: Mailer, W: IssueWriter>(
    deps: &Deps<'_, S, D, M, W>,
    now: DateTime<Utc>,
) -> Result<TickReport, sqlx::Error> {
    let config = deps.config;
    if config.mode == Mode::Off {
        return Ok(TickReport::default());
    }
    let start_at = match (config.missing_for_mode(), config.start_at) {
        (None, Some(start_at)) => start_at,
        (missing, _) => {
            let var = missing.unwrap_or("POSTHOG_AUTOFIX_START_AT");
            tracing::warn!(var, "PostHog auto-fix is not configured; tick skipped");
            return Ok(TickReport {
                errors: vec![format!("{var} is not set")],
                ..TickReport::default()
            });
        }
    };
    // The whole tick runs on the connection that holds the lock: the Lambda
    // pool has a single connection.
    let mut conn = deps.pool.acquire().await?;
    if !lock(&mut conn).await? {
        return Ok(TickReport {
            busy: true,
            ..TickReport::default()
        });
    }
    // Back in the pool with the lock held, the connection would block every
    // later tick. Closed when dropped (a panic included), it takes the lock
    // with it; a tick a minute can afford the reconnect.
    conn.close_on_drop();
    let mut tick = Tick {
        deps,
        conn: &mut conn,
        now,
        start_at,
        report: TickReport::default(),
    };
    let outcome = tick.run().await;
    let report = tick.report;
    if let Err(error) = unlock(&mut conn).await {
        tracing::warn!(
            ?error,
            "could not release the auto-fix lock; closing the connection frees it"
        );
    }
    outcome.map(|()| report)
}

async fn lock(conn: &mut MySqlConnection) -> Result<bool, sqlx::Error> {
    let acquired: Option<i64> = sqlx::query_scalar(LOCK).fetch_one(conn).await?;
    Ok(acquired == Some(1))
}

async fn unlock(conn: &mut MySqlConnection) -> Result<(), sqlx::Error> {
    sqlx::query(UNLOCK).execute(conn).await.map(|_| ())
}

/// A tick in progress, holding the lock on `conn`.
struct Tick<'t, S, D, M, W> {
    deps: &'t Deps<'t, S, D, M, W>,
    conn: &'t mut MySqlConnection,
    now: DateTime<Utc>,
    start_at: DateTime<Utc>,
    report: TickReport,
}

impl<S: IssueSource, D: Dispatcher, M: Mailer, W: IssueWriter> Tick<'_, S, D, M, W> {
    async fn run(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        self.watchdog().await?;
        // PostHog is read once per poll window, however many ticks overlap.
        let poll = policy::window_start(self.now, config.poll_minutes);
        if ledger::claim_notice(&mut *self.conn, "poll", poll).await? {
            self.discover().await?;
            self.storm().await?;
            self.judge_settled().await?;
            self.check_regressions().await?;
            self.assign_opened_fixes().await?;
        }
        match config.mode {
            Mode::Live => self.dispatch().await,
            Mode::DryRun => self.preview().await,
            Mode::Off => Ok(()),
        }
    }

    async fn email(&self, email: &Email) -> bool {
        notify::deliver(self.deps.mailer, &self.deps.config.recipients, email).await
    }

    /// PostHog or GitHub failed: the step is retried on the next poll.
    fn failed(&mut self, step: &str, error: &impl Display) {
        tracing::warn!(step, error = %error, "PostHog auto-fix step failed");
        self.report.errors.push(format!("{step}: {error}"));
    }

    /// Gives up on runs that never reported back.
    async fn watchdog(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let cutoff = self.now - Duration::minutes(config.run_timeout_minutes);
        for id in ledger::stale_in_flight(&mut *self.conn, cutoff).await? {
            if !ledger::time_out(&mut *self.conn, id, cutoff, self.now).await? {
                continue;
            }
            self.report.timed_out += 1;
            self.report.processed += 1;
            if let Some(run) = ledger::load_run(&mut *self.conn, id).await? {
                self.email(&emails::run_timed_out(&run, config)).await;
            }
        }
        Ok(())
    }

    /// Records issues created since the newest one recorded.
    async fn discover(&mut self) -> Result<(), sqlx::Error> {
        let watermark = ledger::watermark(&mut *self.conn)
            .await?
            .unwrap_or(self.start_at);
        let since = (watermark - Duration::minutes(DISCOVER_OVERLAP_MINUTES)).max(self.start_at);
        // A full page of known issues would be read again on every poll and
        // never reach newer ones.
        if self.discover_from(since).await? && since < watermark {
            self.discover_from(watermark).await?;
        }
        Ok(())
    }

    /// Records issues created at or after `since` (never before the cutover);
    /// true when PostHog returned a full page and none of it was new.
    async fn discover_from(&mut self, since: DateTime<Utc>) -> Result<bool, sqlx::Error> {
        let issues = match self.deps.source.issues_since(since, DISCOVER_LIMIT).await {
            Ok(issues) => issues,
            Err(error) => {
                self.failed("discover", &error);
                return Ok(false);
            }
        };
        let new: Vec<&Issue> = issues
            .iter()
            .filter(|issue| issue.created_at >= self.start_at)
            .collect();
        let inserted = ledger::record_discovered(&mut *self.conn, &new, self.now).await?;
        self.report.discovered += inserted;
        self.report.processed += inserted;
        Ok(inserted == 0 && issues.len() >= DISCOVER_LIMIT as usize)
    }

    /// One digest email per storm window with more new issues than the threshold.
    async fn storm(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let since = self.now - Duration::minutes(config.storm_window_minutes);
        let new_issues = ledger::count_created_since(&mut *self.conn, since).await?;
        if new_issues <= config.storm_threshold {
            return Ok(());
        }
        let window = policy::window_start(self.now, config.storm_window_minutes);
        if !ledger::claim_notice(&mut *self.conn, "storm", window).await? {
            return Ok(());
        }
        let top: Vec<(String, usize)> = ledger::top_groups(&mut *self.conn, since, STORM_TOP)
            .await?
            .into_iter()
            .map(|(label, issues)| (label, usize::try_from(issues).unwrap_or(0)))
            .collect();
        let email = emails::storm_digest(
            usize::try_from(new_issues).unwrap_or(usize::MAX),
            config.storm_window_minutes,
            &top,
            config,
        );
        self.report.storm_digest = self.email(&email).await;
        Ok(())
    }

    /// Judges the oldest settled issues: queued, skipped or duplicate.
    async fn judge_settled(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let settled_by = self.now - Duration::minutes(config.settle_minutes);
        let rows = ledger::settled(&mut *self.conn, settled_by, JUDGE_BATCH).await?;
        let Some(oldest) = rows.first().map(|row| row.issue_created_at) else {
            return Ok(());
        };
        let ids: Vec<String> = rows
            .iter()
            .map(|row| row.posthog_issue_id.clone())
            .collect();
        let issues = match self.deps.source.issues(&ids).await {
            Ok(issues) => issues,
            Err(error) => {
                self.failed("settle", &error);
                return Ok(());
            }
        };
        let since = oldest - Duration::hours(EVENTS_LOOKBACK_HOURS);
        let events = match self.deps.source.events(&ids, since).await {
            Ok(events) => events,
            Err(error) => {
                self.failed("settle", &error);
                return Ok(());
            }
        };
        let issues: HashMap<&str, &Issue> = issues
            .iter()
            .map(|issue| (issue.id.as_str(), issue))
            .collect();
        let events: HashMap<&str, &IssueEvents> = events
            .iter()
            .map(|events| (events.issue_id.as_str(), events))
            .collect();
        // One by one: the second issue of a bug in this batch duplicates the first.
        for row in &rows {
            let id = row.posthog_issue_id.as_str();
            self.judge(row.id, issues.get(id).copied(), events.get(id).copied())
                .await?;
        }
        Ok(())
    }

    async fn judge(
        &mut self,
        run_id: u64,
        issue: Option<&Issue>,
        events: Option<&IssueEvents>,
    ) -> Result<(), sqlx::Error> {
        let lib = events.and_then(|events| events.lib.as_deref());
        let source =
            policy::source_of(events.and_then(|events| events.distinct_id.as_deref()), lib);
        let (verdict, signature) = match issue {
            Some(issue) => self.verdict(run_id, issue, source, events).await?,
            None => (Verdict::Skipped(SkipReason::NoEvents), None),
        };
        let judgement = Judgement {
            verdict,
            source,
            lib,
            occurrences: events.map_or(0, |events| events.occurrences),
            users: events.map_or(0, |events| events.users),
            signature: signature.as_deref(),
            issue_status: issue.map(|issue| issue.status.as_str()),
        };
        if !ledger::record_judgement(&mut *self.conn, run_id, &judgement, self.now).await? {
            return Ok(());
        }
        self.report.processed += 1;
        match verdict {
            Verdict::Queued => self.report.queued += 1,
            Verdict::Skipped(_) => self.report.skipped += 1,
            Verdict::Duplicate { .. } => self.report.duplicates += 1,
        }
        Ok(())
    }

    /// The verdict on an issue PostHog still has, and its signature when it
    /// was eligible.
    async fn verdict(
        &mut self,
        run_id: u64,
        issue: &Issue,
        source: Source,
        events: Option<&IssueEvents>,
    ) -> Result<(Verdict, Option<String>), sqlx::Error> {
        let config = self.deps.config;
        let candidate = Candidate {
            issue_status: &issue.status,
            source,
            exception_type: &issue.name,
            message: &issue.description,
            host: events.and_then(|events| events.host.as_deref()),
            occurrences: events.map_or(0, |events| events.occurrences),
        };
        if let Err(reason) = policy::eligibility(&candidate, &config.exclude) {
            return Ok((Verdict::Skipped(reason), None));
        }
        let top_frame = events
            .and_then(|events| events.exception_list.as_ref())
            .and_then(signature::top_frame);
        let signature = signature::signature(&Fingerprint {
            exception_type: &issue.name,
            message: &issue.description,
            source,
            top_frame: top_frame.as_deref(),
        });
        let since = self.now - Duration::days(config.signature_window_days);
        let dry_run = config.mode == Mode::DryRun;
        let anchor = ledger::anchor(&mut *self.conn, &signature, run_id, since, dry_run).await?;
        let verdict = anchor.map_or(Verdict::Queued, |of| Verdict::Duplicate { of });
        Ok((verdict, Some(signature)))
    }

    /// Emails once when PostHog reopens an issue a run fixed.
    async fn check_regressions(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let since = self.now - Duration::days(REGRESSION_DAYS);
        let fixed = ledger::fixed_since(&mut *self.conn, since, REGRESSION_BATCH).await?;
        if fixed.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = fixed
            .iter()
            .map(|row| row.run.posthog_issue_id.clone())
            .collect();
        let issues = match self.deps.source.issues(&ids).await {
            Ok(issues) => issues,
            Err(error) => {
                self.failed("regressions", &error);
                return Ok(());
            }
        };
        let current: HashMap<&str, &str> = issues
            .iter()
            .map(|issue| (issue.id.as_str(), issue.status.as_str()))
            .collect();
        for row in &fixed {
            let Some(&status) = current.get(row.run.posthog_issue_id.as_str()) else {
                continue;
            };
            if policy::is_regression(row.last_issue_status.as_deref(), status) {
                if ledger::mark_regression(&mut *self.conn, row.run.id, status, self.now).await? {
                    self.report.regressions += 1;
                    self.report.processed += 1;
                    self.email(&emails::regression(&row.run, config)).await;
                }
            } else if ledger::store_issue_status(&mut *self.conn, row.run.id, status).await? {
                self.report.processed += 1;
            }
        }
        Ok(())
    }

    /// Write-back: assigns the PostHog issues of opened fixes to their
    /// reviewer. A failure is retried on the next poll.
    async fn assign_opened_fixes(&mut self) -> Result<(), sqlx::Error> {
        if !self.deps.writer.enabled() {
            return Ok(());
        }
        let since = self.now - Duration::days(ASSIGN_DAYS);
        for (id, issue_id) in
            ledger::opened_unassigned(&mut *self.conn, since, ASSIGN_BATCH).await?
        {
            if let Err(error) = self.deps.writer.assign(&issue_id).await {
                self.failed(&format!("assign run {id}"), &error);
                continue;
            }
            if ledger::mark_assigned(&mut *self.conn, id, self.now).await? {
                self.report.assigned += 1;
                self.report.processed += 1;
            }
        }
        Ok(())
    }

    /// Starts the most widespread queued runs the caps allow.
    async fn dispatch(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let in_flight = ledger::count_in_flight(&mut *self.conn).await?;
        let last_day =
            ledger::count_dispatched_since(&mut *self.conn, self.now - Duration::days(1)).await?;
        let slots = policy::dispatch_slots(in_flight, last_day, config);
        if slots == 0 {
            return Ok(());
        }
        for run in ledger::due_queued(&mut *self.conn, self.now, slots).await? {
            if !ledger::claim_dispatch(&mut *self.conn, run.id, self.now).await? {
                continue;
            }
            self.report.processed += 1;
            let dispatch = Dispatch {
                run_id: run.id,
                issue_id: &run.posthog_issue_id,
            };
            match self.deps.dispatcher.dispatch(&dispatch).await {
                Ok(()) => self.report.dispatched += 1,
                Err(error) => self.dispatch_refused(&run, &error).await?,
            }
        }
        Ok(())
    }

    /// Queues `run` again with a backoff, or fails it after its last attempt.
    async fn dispatch_refused(
        &mut self,
        run: &RunView,
        error: &DispatchError,
    ) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        self.failed(&format!("dispatch run {}", run.id), error);
        let message = error.to_string();
        // `run` was read before the claim counted this attempt.
        let attempts = run.attempts.saturating_add(1);
        if attempts < config.max_attempts {
            let retry_at = self.now + policy::retry_delay(attempts);
            ledger::retry_dispatch(&mut *self.conn, run.id, retry_at, &message).await?;
            return Ok(());
        }
        if !ledger::fail_dispatch(&mut *self.conn, run.id, &message, self.now).await? {
            return Ok(());
        }
        if let Some(failed) = ledger::load_run(&mut *self.conn, run.id).await? {
            self.email(&emails::dispatch_failed(&failed, &message, config))
                .await;
        }
        Ok(())
    }

    /// Dry run: moves due queued runs to `previewed` and emails one digest.
    async fn preview(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let mut previewed = Vec::new();
        for run in ledger::due_queued(&mut *self.conn, self.now, PREVIEW_BATCH).await? {
            if !ledger::mark_previewed(&mut *self.conn, run.id).await? {
                continue;
            }
            self.report.previewed += 1;
            self.report.processed += 1;
            if let Some(run) = ledger::load_run(&mut *self.conn, run.id).await? {
                previewed.push(run);
            }
        }
        if !previewed.is_empty() {
            self.email(&emails::preview_digest(&previewed, config))
                .await;
        }
        Ok(())
    }
}
