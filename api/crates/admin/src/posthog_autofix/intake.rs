//! One tick of the intake job: watchdog, discover, settle and judge, check
//! for regressions, then dispatch (live) or preview (dry run).

use super::config::{Config, Mode};
use super::emails;
use super::github::{Dispatch, DispatchError, Dispatcher};
use super::ledger::{self, SettledRow, Sighting, Verdict};
use super::model::{LastSeen, RunView, SkipReason, Source};
use super::notify::{self, Email, Mailer};
use super::policy::{self, Candidate};
use super::posthog::{EventGroup, Issue, IssueSource};
use super::signature::{self, Fingerprint};
use super::writeback::IssueWriter;
use crate::openapi::ToSchema;
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::{Connection, MySqlConnection, MySqlPool};
use std::collections::HashMap;
use std::fmt::Display;

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
    /// New bugs recorded (a bug is one signature of `$exception` events).
    pub discovered: usize,
    /// Bugs judged eligible and queued.
    pub queued: usize,
    /// Bugs skipped as noise, resolved, suppressed or not production.
    pub skipped: usize,
    /// Browser errors folded into the API error of the same request.
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
    /// A storm digest was sent.
    pub storm_digest: bool,
    /// Another tick held the lock; this one did nothing.
    pub busy: bool,
    /// Steps that failed (PostHog or GitHub unreachable); retried next tick.
    pub errors: Vec<String>,
}

/// Event groups read from PostHog per query; a full page is read again over
/// a shorter span.
const GROUP_LIMIT: u32 = 500;
/// A full page halves the span this many times at most.
const GROUP_SPLITS: u32 = 4;
/// Events this recent are left for the next poll: PostHog makes events
/// visible up to this late.
const INGESTION_LAG_MINUTES: i64 = 2;
/// One query reads at most this far, and a first read starts this far back.
const MAX_READ_HOURS: i64 = 24;
/// Settled bugs judged per poll.
const JUDGE_BATCH: u32 = 20;
/// A bug whose run failed or never reported runs again this long after the
/// run ended, if it keeps happening.
const FAILED_RUN_RETRY_HOURS: i64 = 24;
/// A browser 5xx and the API error of its request are this close in time.
const LINK_SLACK_MINUTES: i64 = 10;
/// Events of previewed issues are read from this long before the oldest.
const EVENTS_LOOKBACK_HOURS: i64 = 1;
const STORM_TOP: u32 = 5;
/// A fix is watched for a regression this long after it finished.
const REGRESSION_DAYS: i64 = 30;
const REGRESSION_BATCH: u32 = 100;
/// An opened fix's issue is assigned within this many days, or not at all.
const ASSIGN_DAYS: i64 = 7;
const ASSIGN_BATCH: u32 = 20;
const PREVIEW_BATCH: u32 = 20;
/// The API Gateway cuts the request at 30 s: no new PostHog or GitHub call
/// (10 s at most) starts after this, and the rest waits for the next tick.
const OUT_OF_TIME: &str = "tick time budget reached";
const TICK_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

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
        deadline: std::time::Instant::now() + TICK_BUDGET,
        report: TickReport::default(),
        outbox: Vec::new(),
    };
    let outcome = tick.run().await;
    let Tick { report, outbox, .. } = tick;
    if let Err(error) = unlock(&mut conn).await {
        tracing::warn!(
            ?error,
            "could not release the auto-fix lock; closing the connection frees it"
        );
    }
    // Sent with the lock released: a slow mail server must not hold it, nor
    // keep the connection idle past the database's wait_timeout.
    drop(conn);
    for email in &outbox {
        notify::deliver(deps.mailer, &deps.config.recipients, email).await;
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
    /// No new PostHog or GitHub call starts after this.
    deadline: std::time::Instant,
    report: TickReport,
    /// Emails to send once the lock is released.
    outbox: Vec<Email>,
}

impl<S: IssueSource, D: Dispatcher, M: Mailer, W: IssueWriter> Tick<'_, S, D, M, W> {
    async fn run(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        self.watchdog().await?;
        // PostHog is read once per poll window, however many ticks overlap.
        let poll = policy::window_start(self.now, config.poll_minutes);
        if ledger::claim_notice(&mut *self.conn, "poll", poll, self.now).await? {
            self.discover().await?;
            self.storm().await?;
            if self.out_of_time() {
                return Ok(());
            }
            self.judge_settled().await?;
            if self.out_of_time() {
                return Ok(());
            }
            self.check_regressions().await?;
            self.assign_opened_fixes().await?;
        }
        if self.out_of_time() {
            return Ok(());
        }
        match config.mode {
            Mode::Live => self.dispatch().await,
            Mode::DryRun => self.preview().await,
            Mode::Off => Ok(()),
        }
    }

    fn queue(&mut self, email: Email) {
        self.outbox.push(email);
    }

    /// Past the tick's budget: noted once, and the caller stops starting
    /// PostHog or GitHub calls.
    fn out_of_time(&mut self) -> bool {
        let out = std::time::Instant::now() >= self.deadline;
        if out
            && !self
                .report
                .errors
                .iter()
                .any(|error| error.starts_with(OUT_OF_TIME))
        {
            self.report
                .errors
                .push(format!("{OUT_OF_TIME}; the rest waits for the next tick"));
        }
        out
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
                self.queue(emails::run_timed_out(&run, config));
            }
        }
        Ok(())
    }

    /// Reads the `$exception` events since the cursor and records them as
    /// bugs: counted on the bug's row, or a new row.
    async fn discover(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let first = (self.now - Duration::hours(MAX_READ_HOURS)).max(self.start_at);
        let from = ledger::cursor(&mut *self.conn)
            .await?
            .unwrap_or(first)
            .max(self.start_at);
        let mut to = whole_second(self.now - Duration::minutes(INGESTION_LAG_MINUTES))
            .min(from + Duration::hours(MAX_READ_HOURS));
        if to <= from {
            return Ok(());
        }
        let mut groups;
        let mut splits = 0;
        loop {
            groups = match self.deps.source.event_groups(from, to, GROUP_LIMIT).await {
                Ok(groups) => groups,
                Err(error) => {
                    self.failed("discover", &error);
                    return Ok(());
                }
            };
            let half = whole_second(from + (to - from) / 2);
            if groups.len() < GROUP_LIMIT as usize || splits == GROUP_SPLITS || half <= from {
                break;
            }
            to = half;
            splits += 1;
        }
        if groups.len() >= GROUP_LIMIT as usize {
            tracing::warn!(%from, %to, "PostHog event groups cut at the limit");
        }
        let sightings = sightings(&groups);
        let since = self.now - Duration::days(config.signature_window_days);
        let failed_since = self.now - Duration::hours(FAILED_RUN_RETRY_HOURS);
        // Rows and cursor move together: a read is never counted twice.
        let mut tx = self.conn.begin().await?;
        let mut discovered = 0;
        for sighting in &sightings {
            let row = ledger::bug_row(&mut *tx, &sighting.signature, since, failed_since).await?;
            if let Some(id) = row {
                ledger::count_sighting(&mut *tx, id, sighting).await?;
                ledger::lift_folded_users(&mut *tx, id).await?;
            } else {
                ledger::record_sighting(&mut *tx, sighting, self.now).await?;
                discovered += 1;
            }
        }
        ledger::advance_cursor(&mut *tx, to, self.now).await?;
        tx.commit().await?;
        self.report.discovered += discovered;
        self.report.processed += discovered;
        Ok(())
    }

    /// One digest email when more new bugs than the threshold arrived in the
    /// last storm window, and none sooner than a window after the last.
    async fn storm(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let since = self.now - Duration::minutes(config.storm_window_minutes);
        let new_issues = ledger::count_created_since(&mut *self.conn, since).await?;
        if new_issues <= config.storm_threshold {
            return Ok(());
        }
        // The count slides, so the digest must too: a storm across an aligned
        // window boundary is still one storm.
        if ledger::notice_since(&mut *self.conn, "storm", since).await? {
            return Ok(());
        }
        let window = policy::window_start(self.now, config.storm_window_minutes);
        if !ledger::claim_notice(&mut *self.conn, "storm", window, self.now).await? {
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
        self.report.storm_digest = true;
        self.queue(email);
        Ok(())
    }

    /// Judges the oldest settled bugs: queued, skipped or folded.
    async fn judge_settled(&mut self) -> Result<(), sqlx::Error> {
        let config = self.deps.config;
        let settled_by = self.now - Duration::minutes(config.settle_minutes);
        // Server rows come first: a browser 5xx of this batch folds into its
        // API error.
        let rows = ledger::settled(&mut *self.conn, settled_by, JUDGE_BATCH).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut ids: Vec<String> = rows
            .iter()
            .map(|row| row.posthog_issue_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        let issues = match self.deps.source.issues(&ids).await {
            Ok(issues) => issues,
            Err(error) => {
                self.failed("settle", &error);
                return Ok(());
            }
        };
        let issues: HashMap<&str, &Issue> = issues
            .iter()
            .map(|issue| (issue.id.as_str(), issue))
            .collect();
        for row in &rows {
            let status = issues
                .get(row.posthog_issue_id.as_str())
                .map(|issue| issue.status.as_str());
            self.judge(row, status).await?;
        }
        Ok(())
    }

    async fn judge(
        &mut self,
        row: &SettledRow,
        issue_status: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let verdict = match issue_status {
            Some(status) => self.verdict(row, status).await?,
            None => Some(Verdict::Skipped(SkipReason::NoEvents)),
        };
        // Its API error is not judged yet: folding into a row that may still
        // be skipped would lose the bug, so it waits for the next poll.
        let Some(verdict) = verdict else {
            return Ok(());
        };
        if !ledger::record_judgement(&mut *self.conn, row.id, verdict, issue_status, self.now)
            .await?
        {
            return Ok(());
        }
        if let Verdict::Duplicate { .. } = verdict {
            ledger::lift_folded_users(&mut *self.conn, row.id).await?;
        }
        self.report.processed += 1;
        match verdict {
            Verdict::Queued => self.report.queued += 1,
            Verdict::Skipped(_) => self.report.skipped += 1,
            Verdict::Duplicate { .. } => self.report.duplicates += 1,
        }
        Ok(())
    }

    /// The gates on the bug's own events, then the fold of a browser 5xx
    /// into the API error of the same request; `None` while that API error
    /// waits to be judged.
    async fn verdict(
        &mut self,
        row: &SettledRow,
        issue_status: &str,
    ) -> Result<Option<Verdict>, sqlx::Error> {
        let source = row.source();
        let candidate = Candidate {
            issue_status,
            source,
            exception_type: &row.issue_name,
            message: &row.issue_description,
            // Discovery drops browser events of other hosts.
            host: None,
            occurrences: row.occurrences.map_or(0, u64::from),
            error_source: row.error_source.as_deref(),
            api_status: row.api_status,
        };
        if let Err(reason) = policy::eligibility(&candidate, &self.deps.config.exclude) {
            return Ok(Some(Verdict::Skipped(reason)));
        }
        let path = policy::web_5xx_path(
            source,
            &row.issue_name,
            &row.issue_description,
            row.api_status,
            row.api_route.as_deref(),
        );
        let (Some(path), Some(first), Some(last)) = (path, row.first_seen_at, row.last_seen_at)
        else {
            return Ok(Some(Verdict::Queued));
        };
        let slack = Duration::minutes(LINK_SLACK_MINUTES);
        let api_rows =
            ledger::api_rows_seen_between(&mut *self.conn, first - slack, last + slack).await?;
        let matching: Vec<_> = api_rows
            .iter()
            .filter(|(_, route, _)| policy::route_matches(route, path))
            .collect();
        if matching.iter().any(|&&(_, _, waiting)| waiting) {
            return Ok(None);
        }
        Ok(Some(
            matching
                .first()
                .map_or(Verdict::Queued, |&&(of, _, _)| Verdict::Duplicate { of }),
        ))
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
                    self.queue(emails::regression(&row.run, config));
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
            if self.out_of_time() {
                break;
            }
            if let Err(error) = self.deps.writer.assign(id, &issue_id).await {
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

    /// Starts the queued runs the caps allow: bugs seen in the last day
    /// first, then the most widespread.
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
            if self.out_of_time() {
                break;
            }
            if !ledger::claim_dispatch(&mut *self.conn, run.id, self.now).await? {
                continue;
            }
            self.report.processed += 1;
            let dispatch = Dispatch {
                run_id: run.id,
                issue_id: &run.posthog_issue_id,
                event_id: run.event_uuid.as_deref(),
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
            self.queue(emails::dispatch_failed(&failed, &message, config));
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
            let seen = self.last_seen(&previewed).await?;
            self.queue(emails::preview_digest(&previewed, &seen, config));
        }
        Ok(())
    }

    /// Where and to whom each run's error last happened, by PostHog issue id:
    /// the page or request from PostHog, the user from `users`. Empty when
    /// PostHog cannot be read in time; the digest goes out without it.
    async fn last_seen(
        &mut self,
        runs: &[RunView],
    ) -> Result<HashMap<String, LastSeen>, sqlx::Error> {
        let Some(oldest) = runs.iter().map(|run| run.issue_created_at).min() else {
            return Ok(HashMap::new());
        };
        if self.out_of_time() {
            return Ok(HashMap::new());
        }
        let ids: Vec<String> = runs
            .iter()
            .map(|run| run.posthog_issue_id.clone())
            .collect();
        let since = oldest - Duration::hours(EVENTS_LOOKBACK_HOURS);
        let events = match self.deps.source.events(&ids, since).await {
            Ok(events) => events,
            Err(error) => {
                self.failed("preview details", &error);
                return Ok(HashMap::new());
            }
        };
        let mut seen = HashMap::new();
        for events in events {
            // Signed-in browsers report as the user id; servers and signed-out
            // pages as anything else.
            let user_id = events
                .distinct_id
                .as_deref()
                .and_then(|id| id.parse::<i32>().ok());
            let (user_name, company_id) = match user_id {
                Some(id) => ledger::user(&mut *self.conn, id).await?.unwrap_or_default(),
                None => (None, None),
            };
            seen.insert(
                events.issue_id,
                LastSeen {
                    url: events.url,
                    request: events.request,
                    user_id,
                    user_name,
                    company_id,
                },
            );
        }
        Ok(seen)
    }
}

/// `at` without its fraction of a second: the cursor is stored in whole
/// seconds, so every read ends on one.
fn whole_second(at: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
}

/// A read's groups as bugs: browser groups of other hosts dropped (never
/// recorded), the rest merged by signature. The latest event's group gives
/// the sample issue, event and request; counts add up, users take the
/// largest (the same person may be in several groups).
fn sightings(groups: &[EventGroup]) -> Vec<Sighting> {
    let mut bugs: Vec<Sighting> = Vec::new();
    for group in groups {
        let source = policy::source_of(group.sender.as_deref(), group.lib.as_deref());
        if source == Source::Web
            && group
                .host
                .as_deref()
                .is_some_and(policy::is_non_production_host)
        {
            continue;
        }
        let top_frame = group.exception_list.as_ref().and_then(signature::top_frame);
        let signature = signature::signature(&Fingerprint {
            exception_type: &group.exception_type,
            message: &group.message,
            source,
            top_frame: top_frame.as_deref(),
        });
        let sighting = Sighting {
            signature,
            issue_id: group.issue_id.clone(),
            event_uuid: group.event_uuid.clone(),
            exception_type: group.exception_type.clone(),
            message: group.message.clone(),
            source,
            lib: group.lib.clone(),
            api_route: group.route.clone(),
            api_status: group.api_status,
            error_source: group.error_source.clone(),
            occurrences: group.occurrences,
            users: group.users,
            first_seen: group.first_seen,
            last_seen: group.last_seen,
        };
        let Some(bug) = bugs
            .iter_mut()
            .find(|bug| bug.signature == sighting.signature)
        else {
            bugs.push(sighting);
            continue;
        };
        let occurrences = bug.occurrences.saturating_add(sighting.occurrences);
        let users = bug.users.max(sighting.users);
        let first_seen = bug.first_seen.min(sighting.first_seen);
        if sighting.last_seen > bug.last_seen {
            *bug = sighting;
        }
        bug.occurrences = occurrences;
        bug.users = users;
        bug.first_seen = first_seen;
    }
    bugs
}
