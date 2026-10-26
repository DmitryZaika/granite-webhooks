//! SQL over `posthog_autofix_runs` and `posthog_autofix_notices`, and the
//! `users` lookup the dry-run email needs.
//!
//! Every status change is one conditional `UPDATE ... WHERE status IN (...)`
//! built from [`may_become`], so the state machine in [`Status::can_become`]
//! is enforced by the database even when two ticks or two callbacks race:
//! only the statement that changed the row (`rows_affected() == 1`) acts on
//! the change, e.g. sends its email.

use super::model::{RunView, SkipReason, Source, Status};
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::MySqlExecutor;

/// `status IN ('a', 'b')`: the statuses a row may leave to become `next`.
/// The values are the enum's constants, never input.
pub fn may_become(next: Status) -> String {
    let sources: Vec<String> = Status::sources_of(next)
        .into_iter()
        .map(|status| format!("'{}'", status.as_str()))
        .collect();
    format!("status IN ({})", sources.join(", "))
}

/// [`may_become`] narrowed to rows now in `from`, for a move whose target
/// has other sources: `dispatched -> queued` must not catch a `running` row.
pub fn moves(from: Status, next: Status) -> String {
    debug_assert!(from.can_become(next), "{from:?} cannot become {next:?}");
    format!("{} AND status = '{}'", may_become(next), from.as_str())
}

pub async fn load_run(db: impl MySqlExecutor<'_>, id: u64) -> Result<Option<RunView>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM posthog_autofix_runs WHERE id = ?",
        RunView::COLUMNS
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

/// The name and company of user `id`; `None` when there is no such user.
pub async fn user(
    db: impl MySqlExecutor<'_>,
    id: i32,
) -> Result<Option<(Option<String>, Option<i32>)>, sqlx::Error> {
    sqlx::query_as("SELECT name, company_id FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
}

/// Claims once-per-window work (`kind` at `window_start`): true for the one
/// caller that created the notice row. The row is stamped `now`, the tick's
/// clock, so [`notice_since`] works with injected time.
pub async fn claim_notice(
    db: impl MySqlExecutor<'_>,
    kind: &str,
    window_start: chrono::NaiveDateTime,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT IGNORE INTO posthog_autofix_notices (kind, window_start, created_at) \
         VALUES (?, ?, ?)",
    )
    .bind(kind)
    .bind(window_start)
    .bind(now.naive_utc())
    .execute(db)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// Whether a `kind` notice was claimed after `since`: a rule over a sliding
/// span, which the aligned window of [`claim_notice`] cannot give.
pub async fn notice_since(
    db: impl MySqlExecutor<'_>,
    kind: &str,
    since: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM posthog_autofix_notices WHERE kind = ? AND created_at > ?)",
    )
    .bind(kind)
    .bind(since.naive_utc())
    .fetch_one(db)
    .await
}

// Column widths, in characters (utf8mb4 VARCHARs count characters).
const NAME_CHARS: usize = 255;
const DESCRIPTION_CHARS: usize = 2000;
const LIB_CHARS: usize = 64;
const ISSUE_STATUS_CHARS: usize = 16;
const ROUTE_CHARS: usize = 255;
const ERROR_SOURCE_CHARS: usize = 32;
const ERROR_CHARS: usize = 512;

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn count(rows: i64) -> u32 {
    u32::try_from(rows).unwrap_or(u32::MAX)
}

/// The read cursor's notice kind: one row per successful read of events.
const CURSOR: &str = "events_read";

/// Where the next read of events starts: the end of the last one.
pub async fn cursor(db: impl MySqlExecutor<'_>) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let end: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT MAX(window_start) FROM posthog_autofix_notices WHERE kind = ?")
            .bind(CURSOR)
            .fetch_one(db)
            .await?;
    Ok(end.map(|at| at.and_utc()))
}

/// Records that events before `to` (whole seconds) were read.
pub async fn advance_cursor(
    db: impl MySqlExecutor<'_>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    claim_notice(db, CURSOR, to.naive_utc(), now)
        .await
        .map(|_| ())
}

/// One bug's events of one read, merged by signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub signature: String,
    /// The issue and event of the latest event.
    pub issue_id: String,
    pub event_uuid: Option<String>,
    pub exception_type: String,
    pub message: String,
    pub source: Source,
    pub lib: Option<String>,
    pub api_route: Option<String>,
    pub api_status: Option<u16>,
    pub error_source: Option<String>,
    pub occurrences: u64,
    pub users: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// The row that counts the events of the bug with `signature`.
///
/// It is one whose last event is at or after `since` (a bug that keeps
/// happening keeps its row), or one still waiting or in flight. A row folded
/// into another is only taken when it is the bug's only row.
pub async fn bug_row(
    db: impl MySqlExecutor<'_>,
    signature: &str,
    since: DateTime<Utc>,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM posthog_autofix_runs
          WHERE signature = ?
            AND (COALESCE(last_seen_at, discovered_at) >= ?
                 OR status IN ('discovered', 'queued', 'dispatched', 'running'))
          ORDER BY status = 'duplicate', id DESC LIMIT 1",
    )
    .bind(signature)
    .bind(since.naive_utc())
    .fetch_optional(db)
    .await
}

/// Adds a read's events to row `id`. A row not yet started takes the latest
/// event as its sample, so the fix run starts from the freshest evidence.
pub async fn count_sighting(
    db: impl MySqlExecutor<'_>,
    id: u64,
    sighting: &Sighting,
) -> Result<(), sqlx::Error> {
    // MySQL assigns left to right: both samples compare against the old
    // last_seen_at, before it moves.
    const NEWER: &str = "status IN ('discovered', 'queued') \
        AND ? >= COALESCE(last_seen_at, issue_created_at)";
    let last_seen = sighting.last_seen.naive_utc();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET posthog_issue_id = IF({NEWER}, ?, posthog_issue_id),
                event_uuid = IF({NEWER}, COALESCE(?, event_uuid), event_uuid),
                occurrences = COALESCE(occurrences, 0) + ?,
                users = GREATEST(COALESCE(users, 0), ?),
                first_seen_at = LEAST(COALESCE(first_seen_at, ?), ?),
                last_seen_at = GREATEST(COALESCE(last_seen_at, ?), ?)
          WHERE id = ?"
    )))
    .bind(last_seen)
    .bind(&sighting.issue_id)
    .bind(last_seen)
    .bind(sighting.event_uuid.as_deref())
    .bind(sighting.occurrences)
    .bind(sighting.users)
    .bind(sighting.first_seen.naive_utc())
    .bind(sighting.first_seen.naive_utc())
    .bind(last_seen)
    .bind(last_seen)
    .bind(id)
    .execute(db)
    .await
    .map(|_| ())
}

/// Records a new bug as `discovered`; its first event starts the settle clock.
pub async fn record_sighting(
    db: impl MySqlExecutor<'_>,
    sighting: &Sighting,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO posthog_autofix_runs
           (posthog_issue_id, event_uuid, issue_created_at, first_seen_at, last_seen_at,
            issue_name, issue_description, source, lib, api_route, api_status, error_source,
            occurrences, users, signature, discovered_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&sighting.issue_id)
    .bind(sighting.event_uuid.as_deref())
    .bind(sighting.first_seen.naive_utc())
    .bind(sighting.first_seen.naive_utc())
    .bind(sighting.last_seen.naive_utc())
    .bind(clip(&sighting.exception_type, NAME_CHARS))
    .bind(clip(&sighting.message, DESCRIPTION_CHARS))
    .bind(sighting.source.as_str())
    .bind(sighting.lib.as_deref().map(|lib| clip(lib, LIB_CHARS)))
    .bind(
        sighting
            .api_route
            .as_deref()
            .map(|route| clip(route, ROUTE_CHARS)),
    )
    .bind(sighting.api_status)
    .bind(
        sighting
            .error_source
            .as_deref()
            .map(|source| clip(source, ERROR_SOURCE_CHARS)),
    )
    .bind(u32::try_from(sighting.occurrences).unwrap_or(u32::MAX))
    .bind(u32::try_from(sighting.users).unwrap_or(u32::MAX))
    .bind(&sighting.signature)
    .bind(now.naive_utc())
    .execute(db)
    .await
    .map(|_| ())
}

/// Lifts the users of the row that row `id` was folded into to `id`'s own:
/// a server error has no user id, so its reach is that of its browser copy.
pub async fn lift_folded_users(db: impl MySqlExecutor<'_>, id: u64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE posthog_autofix_runs target
           JOIN posthog_autofix_runs copy ON copy.duplicate_of_run_id = target.id
            SET target.users = GREATEST(COALESCE(target.users, 0), COALESCE(copy.users, 0))
          WHERE copy.id = ? AND copy.status = 'duplicate'",
    )
    .bind(id)
    .execute(db)
    .await
    .map(|_| ())
}

/// Bugs first seen at or after `since`.
pub async fn count_created_since(
    db: impl MySqlExecutor<'_>,
    since: DateTime<Utc>,
) -> Result<u32, sqlx::Error> {
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM posthog_autofix_runs WHERE issue_created_at >= ?")
            .bind(since.naive_utc())
            .fetch_one(db)
            .await?;
    Ok(count(rows))
}

/// The most common `name: description` (description cut to 80 characters)
/// among bugs first seen at or after `since`, with their counts.
pub async fn top_groups(
    db: impl MySqlExecutor<'_>,
    since: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT CONCAT(issue_name, ': ', LEFT(issue_description, 80)) AS label, COUNT(*) AS issues
           FROM posthog_autofix_runs WHERE issue_created_at >= ?
          GROUP BY label ORDER BY issues DESC, label LIMIT ?",
    )
    .bind(since.naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

/// Runs dispatched or running since before `cutoff`.
pub async fn stale_in_flight(
    db: impl MySqlExecutor<'_>,
    cutoff: DateTime<Utc>,
) -> Result<Vec<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM posthog_autofix_runs
          WHERE status IN ('dispatched', 'running') AND COALESCE(started_at, dispatched_at) < ?
          ORDER BY id",
    )
    .bind(cutoff.naive_utc())
    .fetch_all(db)
    .await
}

/// The watchdog gives up on a run still in flight since before `cutoff`.
pub async fn time_out(
    db: impl MySqlExecutor<'_>,
    id: u64,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs SET status = 'timed_out', finished_at = ?
          WHERE id = ? AND {} AND COALESCE(started_at, dispatched_at) < ?",
        may_become(Status::TimedOut)
    )))
    .bind(now.naive_utc())
    .bind(id)
    .bind(cutoff.naive_utc())
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// A discovered row, as the judge reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SettledRow {
    pub id: u64,
    pub posthog_issue_id: String,
    pub issue_name: String,
    pub issue_description: String,
    pub source: Option<String>,
    pub occurrences: Option<u32>,
    pub users: Option<u32>,
    pub api_route: Option<String>,
    pub api_status: Option<u16>,
    pub error_source: Option<String>,
    pub first_seen_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl SettledRow {
    pub fn source(&self) -> Source {
        self.source
            .as_deref()
            .and_then(Source::parse)
            .unwrap_or(Source::Unknown)
    }
}

/// Discovered rows first seen at or before `created_by`, oldest first.
pub async fn settled(
    db: impl MySqlExecutor<'_>,
    created_by: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<SettledRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, posthog_issue_id, issue_name, issue_description,
                CAST(source AS CHAR) AS source, occurrences, users, api_route, api_status,
                error_source, first_seen_at, last_seen_at
           FROM posthog_autofix_runs
          WHERE status = 'discovered' AND issue_created_at <= ?
          ORDER BY issue_created_at, id LIMIT ?",
    )
    .bind(created_by.naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

/// API rows (`granite-api`) not skipped or folded whose events span reaches
/// into `[from, to]`, with their routes and whether they still wait to be
/// judged: where a browser 5xx of that time may come from.
pub async fn api_rows_seen_between(
    db: impl MySqlExecutor<'_>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(u64, String, bool)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, api_route, status = 'discovered' FROM posthog_autofix_runs
          WHERE source = 'granite-api' AND api_route IS NOT NULL
            AND status NOT IN ('skipped', 'duplicate')
            AND first_seen_at <= ? AND last_seen_at >= ?
          ORDER BY id",
    )
    .bind(to.naive_utc())
    .bind(from.naive_utc())
    .fetch_all(db)
    .await
}

/// Where a settled bug goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Queued,
    Skipped(SkipReason),
    /// The same bug as run `of`.
    Duplicate {
        of: u64,
    },
}

impl Verdict {
    pub const fn status(self) -> Status {
        match self {
            Self::Queued => Status::Queued,
            Self::Skipped(_) => Status::Skipped,
            Self::Duplicate { .. } => Status::Duplicate,
        }
    }
}

/// `discovered` -> the verdict's status, storing the issue's PostHog status
/// (`None` when PostHog no longer has it).
pub async fn record_judgement(
    db: impl MySqlExecutor<'_>,
    id: u64,
    verdict: Verdict,
    issue_status: Option<&str>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let next = verdict.status();
    let (skip_reason, duplicate_of) = match verdict {
        Verdict::Queued => (None, None),
        Verdict::Skipped(reason) => (Some(reason.as_str()), None),
        Verdict::Duplicate { of } => (None, Some(of)),
    };
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET status = ?, skip_reason = ?, duplicate_of_run_id = ?, last_issue_status = ?,
                enriched_at = ?
          WHERE id = ? AND {}",
        moves(Status::Discovered, next)
    )))
    .bind(next.as_str())
    .bind(skip_reason)
    .bind(duplicate_of)
    .bind(issue_status.map(|status| clip(status, ISSUE_STATUS_CHARS)))
    .bind(now.naive_utc())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// A fixed run and the issue status PostHog last had.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FixedRun {
    #[sqlx(flatten)]
    pub run: RunView,
    pub last_issue_status: Option<String>,
}

/// Runs fixed at or after `since` whose regression was not yet emailed.
pub async fn fixed_since(
    db: impl MySqlExecutor<'_>,
    since: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<FixedRun>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {}, last_issue_status FROM posthog_autofix_runs
          WHERE status = 'succeeded' AND outcome IN ('fixed_with_test', 'fixed_without_test')
            AND finished_at >= ? AND regression_notified_at IS NULL
          ORDER BY id LIMIT ?",
        RunView::COLUMNS
    )))
    .bind(since.naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

/// Runs whose pull request opened since `since` and whose PostHog issue is not
/// assigned yet: `(id, posthog_issue_id)`, oldest first.
pub async fn opened_unassigned(
    db: impl MySqlExecutor<'_>,
    since: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<(u64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, posthog_issue_id FROM posthog_autofix_runs
          WHERE status = 'succeeded' AND pr_url IS NOT NULL AND posthog_assigned_at IS NULL
            AND finished_at >= ?
          ORDER BY finished_at, id LIMIT ?",
    )
    .bind(since.naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

pub async fn mark_assigned(
    db: impl MySqlExecutor<'_>,
    id: u64,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE posthog_autofix_runs SET posthog_assigned_at = ?
          WHERE id = ? AND posthog_assigned_at IS NULL",
    )
    .bind(now.naive_utc())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Claims a run's one regression email, storing the issue's status.
pub async fn mark_regression(
    db: impl MySqlExecutor<'_>,
    id: u64,
    issue_status: &str,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE posthog_autofix_runs SET regression_notified_at = ?, last_issue_status = ?
          WHERE id = ? AND regression_notified_at IS NULL",
    )
    .bind(now.naive_utc())
    .bind(clip(issue_status, ISSUE_STATUS_CHARS))
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Stores the issue's PostHog status; false when it was already stored.
pub async fn store_issue_status(
    db: impl MySqlExecutor<'_>,
    id: u64,
    issue_status: &str,
) -> Result<bool, sqlx::Error> {
    let status = clip(issue_status, ISSUE_STATUS_CHARS);
    let changed = sqlx::query(
        "UPDATE posthog_autofix_runs SET last_issue_status = ?
          WHERE id = ? AND NOT (last_issue_status <=> ?)",
    )
    .bind(&status)
    .bind(id)
    .bind(&status)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// Runs dispatched or running.
pub async fn count_in_flight(db: impl MySqlExecutor<'_>) -> Result<u32, sqlx::Error> {
    let rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM posthog_autofix_runs WHERE status IN ('dispatched', 'running')",
    )
    .fetch_one(db)
    .await?;
    Ok(count(rows))
}

/// Runs dispatched at or after `since`.
pub async fn count_dispatched_since(
    db: impl MySqlExecutor<'_>,
    since: DateTime<Utc>,
) -> Result<u32, sqlx::Error> {
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM posthog_autofix_runs WHERE dispatched_at >= ?")
            .bind(since.naive_utc())
            .fetch_one(db)
            .await?;
    Ok(count(rows))
}

/// Queued rows whose backoff has passed: bugs seen in the last day first,
/// then the most widespread.
pub async fn due_queued(
    db: impl MySqlExecutor<'_>,
    now: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<RunView>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM posthog_autofix_runs
          WHERE status = 'queued' AND (retry_at IS NULL OR retry_at <= ?)
          ORDER BY last_seen_at >= ? DESC, users DESC, occurrences DESC, id LIMIT ?",
        RunView::COLUMNS
    )))
    .bind(now.naive_utc())
    .bind((now - chrono::Duration::days(1)).naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

/// `queued` -> `dispatched`, counting the attempt.
pub async fn claim_dispatch(
    db: impl MySqlExecutor<'_>,
    id: u64,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET status = 'dispatched', dispatched_at = ?, attempts = attempts + 1, retry_at = NULL
          WHERE id = ? AND {}",
        may_become(Status::Dispatched)
    )))
    .bind(now.naive_utc())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

// A refused dispatch started nothing, so both moves below clear
// `dispatched_at`: the row does not count against the daily cap.

/// `dispatched` -> `queued` after a refused dispatch, retried at `retry_at`.
pub async fn retry_dispatch(
    db: impl MySqlExecutor<'_>,
    id: u64,
    retry_at: DateTime<Utc>,
    error: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET status = 'queued', retry_at = ?, last_error = ?, dispatched_at = NULL
          WHERE id = ? AND {}",
        moves(Status::Dispatched, Status::Queued)
    )))
    .bind(retry_at.naive_utc())
    .bind(clip(error, ERROR_CHARS))
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// `dispatched` -> `failed` after the last refused dispatch.
pub async fn fail_dispatch(
    db: impl MySqlExecutor<'_>,
    id: u64,
    error: &str,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET status = 'failed', last_error = ?, finished_at = ?, dispatched_at = NULL
          WHERE id = ? AND {}",
        moves(Status::Dispatched, Status::Failed)
    )))
    .bind(clip(error, ERROR_CHARS))
    .bind(now.naive_utc())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// `queued` -> `previewed` (dry run).
pub async fn mark_previewed(db: impl MySqlExecutor<'_>, id: u64) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs SET status = 'previewed' WHERE id = ? AND {}",
        may_become(Status::Previewed)
    )))
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_are_guarded_by_their_source_statuses() {
        assert_eq!(
            may_become(Status::Succeeded),
            "status IN ('running', 'timed_out')"
        );
        assert_eq!(may_become(Status::Running), "status IN ('dispatched')");
    }

    #[test]
    fn a_retry_leaves_only_a_dispatched_row() {
        assert_eq!(
            moves(Status::Dispatched, Status::Queued),
            "status IN ('discovered', 'dispatched', 'running') AND status = 'dispatched'"
        );
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        assert_eq!(clip("Ошибка", 3), "Оши");
        assert_eq!(clip("ok", 255), "ok");
    }
}
