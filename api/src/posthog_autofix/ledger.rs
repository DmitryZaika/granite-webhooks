//! SQL over `posthog_autofix_runs` and `posthog_autofix_notices`.
//!
//! Every status change is one conditional `UPDATE ... WHERE status IN (...)`
//! built from [`may_become`], so the state machine in [`Status::can_become`]
//! is enforced by the database even when two ticks or two callbacks race:
//! only the statement that changed the row (`rows_affected() == 1`) acts on
//! the change, e.g. sends its email.

use super::model::{RunView, SkipReason, Source, Status};
use super::posthog::Issue;
use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{MySql, MySqlExecutor, QueryBuilder};

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

/// Claims once-per-window work (`kind` at `window_start`): true for the one
/// caller that created the notice row.
pub async fn claim_notice(
    db: impl MySqlExecutor<'_>,
    kind: &str,
    window_start: chrono::NaiveDateTime,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT IGNORE INTO posthog_autofix_notices (kind, window_start) VALUES (?, ?)",
    )
    .bind(kind)
    .bind(window_start)
    .execute(db)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

// Column widths, in characters (utf8mb4 VARCHARs count characters).
const NAME_CHARS: usize = 255;
const DESCRIPTION_CHARS: usize = 2000;
const LIB_CHARS: usize = 64;
const ISSUE_STATUS_CHARS: usize = 16;
const ERROR_CHARS: usize = 512;

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn count(rows: i64) -> u32 {
    u32::try_from(rows).unwrap_or(u32::MAX)
}

/// The newest `issue_created_at` recorded: where discovery resumes.
pub async fn watermark(db: impl MySqlExecutor<'_>) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let newest: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT MAX(issue_created_at) FROM posthog_autofix_runs")
            .fetch_one(db)
            .await?;
    Ok(newest.map(|at| at.and_utc()))
}

/// Records issues as `discovered` and returns how many were new; an issue
/// already in the ledger is left as it is.
pub async fn record_discovered(
    db: impl MySqlExecutor<'_>,
    issues: &[&Issue],
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    if issues.is_empty() {
        return Ok(0);
    }
    let mut insert: QueryBuilder<MySql> = QueryBuilder::new(
        "INSERT IGNORE INTO posthog_autofix_runs \
         (posthog_issue_id, issue_created_at, issue_name, issue_description, discovered_at) ",
    );
    insert.push_values(issues, |mut row, issue| {
        row.push_bind(issue.id.clone())
            .push_bind(issue.created_at.naive_utc())
            .push_bind(clip(&issue.name, NAME_CHARS))
            .push_bind(clip(&issue.description, DESCRIPTION_CHARS))
            .push_bind(now.naive_utc());
    });
    let inserted = insert.build().execute(db).await?.rows_affected();
    Ok(usize::try_from(inserted).unwrap_or(usize::MAX))
}

/// Issues created at or after `since`.
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
/// among issues created at or after `since`, with their issue counts.
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

/// Discovered rows created at or before `created_by`, oldest first.
pub async fn settled(
    db: impl MySqlExecutor<'_>,
    created_by: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<RunView>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM posthog_autofix_runs
          WHERE status = 'discovered' AND issue_created_at <= ?
          ORDER BY issue_created_at, id LIMIT ?",
        RunView::COLUMNS
    )))
    .bind(created_by.naive_utc())
    .bind(limit)
    .fetch_all(db)
    .await
}

/// Where a settled issue goes.
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

/// A settled issue's verdict and what was read about it.
#[derive(Debug, Clone, Copy)]
pub struct Judgement<'a> {
    pub verdict: Verdict,
    pub source: Source,
    pub lib: Option<&'a str>,
    pub occurrences: u64,
    pub users: u64,
    /// `None` when the issue was skipped before its signature was computed.
    pub signature: Option<&'a str>,
    /// The issue's PostHog status; `None` when PostHog no longer has it.
    pub issue_status: Option<&'a str>,
}

/// `discovered` -> the verdict's status, storing what was read.
pub async fn record_judgement(
    db: impl MySqlExecutor<'_>,
    id: u64,
    judgement: &Judgement<'_>,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let next = judgement.verdict.status();
    let (skip_reason, duplicate_of) = match judgement.verdict {
        Verdict::Queued => (None, None),
        Verdict::Skipped(reason) => (Some(reason.as_str()), None),
        Verdict::Duplicate { of } => (None, Some(of)),
    };
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs
            SET status = ?, skip_reason = ?, duplicate_of_run_id = ?, source = ?, lib = ?,
                occurrences = ?, users = ?, signature = ?, last_issue_status = ?, enriched_at = ?
          WHERE id = ? AND {}",
        moves(Status::Discovered, next)
    )))
    .bind(next.as_str())
    .bind(skip_reason)
    .bind(duplicate_of)
    .bind(judgement.source.as_str())
    .bind(judgement.lib.map(|lib| clip(lib, LIB_CHARS)))
    .bind(u32::try_from(judgement.occurrences).unwrap_or(u32::MAX))
    .bind(u32::try_from(judgement.users).unwrap_or(u32::MAX))
    .bind(judgement.signature)
    .bind(
        judgement
            .issue_status
            .map(|status| clip(status, ISSUE_STATUS_CHARS)),
    )
    .bind(now.naive_utc())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

/// The run that row `id`, with `signature`, duplicates.
///
/// The oldest other row of that bug that is waiting or in flight, or ended
/// at or after `since`, or (with `previewed`, a dry run) was previewed at or
/// after it.
pub async fn anchor(
    db: impl MySqlExecutor<'_>,
    signature: &str,
    id: u64,
    since: DateTime<Utc>,
    previewed: bool,
) -> Result<Option<u64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM posthog_autofix_runs
          WHERE signature = ? AND id <> ?
            AND (status IN ('queued', 'dispatched', 'running')
                 OR (status IN ('succeeded', 'failed', 'timed_out') AND finished_at >= ?)
                 OR (? AND status = 'previewed' AND enriched_at >= ?))
          ORDER BY id LIMIT 1",
    )
    .bind(signature)
    .bind(id)
    .bind(since.naive_utc())
    .bind(previewed)
    .bind(since.naive_utc())
    .fetch_optional(db)
    .await
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

/// Queued rows whose backoff has passed, the most widespread first.
pub async fn due_queued(
    db: impl MySqlExecutor<'_>,
    now: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<RunView>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM posthog_autofix_runs
          WHERE status = 'queued' AND (retry_at IS NULL OR retry_at <= ?)
          ORDER BY users DESC, occurrences DESC, id LIMIT ?",
        RunView::COLUMNS
    )))
    .bind(now.naive_utc())
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
