//! The workflow's callbacks: a run started, a run finished.
//!
//! Both are idempotent. A repeated callback finds the row already moved and
//! changes nothing; only the call that moved the row sends the email.

use super::config::Config;
use super::emails;
use super::ledger::{load_run, may_become};
use super::model::{Conclusion, RunReport, RunView, StartedReport, Status, TargetRepo};
use super::notify::{Mailer, deliver};
use super::posthog::is_issue_id;
use crate::routes::jobs::constant_time_eq;
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use sqlx::MySqlPool;

const GITHUB_PREFIX: &str = "https://github.com/";
const URL_MAX: usize = 512;
const NOTES_MAX: usize = 512;
/// A usage-limited run waits this long before the intake dispatches it again.
const USAGE_LIMIT_RETRY_MINUTES: i64 = 60;
const OUT_OF_ATTEMPTS: &str = "usage limit, out of attempts";

/// Whether a callback changed the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Changed,
    /// Already recorded (a retried callback).
    Unchanged,
}

#[derive(Debug, thiserror::Error)]
pub enum CallbackError {
    /// The body is malformed: a bad id, a link that is not GitHub's, a
    /// completed run without the agent's answer.
    #[error("{0}")]
    Invalid(String),
    /// No such run, or its issue id differs from the body's.
    #[error("not found")]
    NotFound,
    /// The run is in a status this callback cannot move it from.
    #[error("run is {0}")]
    Conflict(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// `Authorization: Bearer <POSTHOG_AUTOFIX_CALLBACK_SECRET>`, compared in
/// constant time. Refused when the secret is unset.
pub fn authorized(config: &Config, authorization: Option<&str>) -> bool {
    let (Some(secret), Some(header)) = (config.callback_secret.as_deref(), authorization) else {
        return false;
    };
    header
        .strip_prefix("Bearer ")
        .is_some_and(|sent| constant_time_eq(secret.as_bytes(), sent.as_bytes()))
}

/// `dispatched` -> `running`, recording the GitHub run.
pub async fn record_started(
    pool: &MySqlPool,
    run_id: u64,
    report: &StartedReport,
    now: DateTime<Utc>,
) -> Result<Applied, CallbackError> {
    validate_issue_id(&report.issue_id)?;
    validate_github_url("github_run_url", &report.github_run_url)?;
    let run = load_checked(pool, run_id, &report.issue_id).await?;
    if let Some(applied) = started_verdict(&run, &report.github_run_url)? {
        return Ok(applied);
    }
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs SET status = 'running', started_at = ?, \
         github_run_url = ? WHERE id = ? AND {}",
        may_become(Status::Running)
    )))
    .bind(now)
    .bind(&report.github_run_url)
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected();
    if changed == 1 {
        return Ok(Applied::Changed);
    }
    // Lost a race: the retry that won may be this very report.
    let run = load_checked(pool, run_id, &report.issue_id).await?;
    started_verdict(&run, &report.github_run_url)?
        .ok_or_else(|| CallbackError::Conflict(run.status))
}

/// `Some` when no write is needed or allowed: the run already started with
/// this GitHub run (`Unchanged`) or is in a status that cannot start.
fn started_verdict(run: &RunView, github_run_url: &str) -> Result<Option<Applied>, CallbackError> {
    match run.status() {
        Some(Status::Running) if run.github_run_url.as_deref() == Some(github_run_url) => {
            Ok(Some(Applied::Unchanged))
        }
        Some(Status::Dispatched) => Ok(None),
        _ => Err(CallbackError::Conflict(run.status.clone())),
    }
}

/// Closes the run with its result and emails the developers.
pub async fn record_result(
    pool: &MySqlPool,
    mailer: &impl Mailer,
    config: &Config,
    run_id: u64,
    report: &RunReport,
    now: DateTime<Utc>,
) -> Result<Applied, CallbackError> {
    validate_report(report)?;
    let run = load_checked(pool, run_id, &report.issue_id).await?;
    if is_finished(&run) {
        return Ok(Applied::Unchanged);
    }
    let (next, report) = decide(&run, report, config);
    let changed = if next == Status::Queued {
        requeue(pool, run_id, now).await?
    } else {
        close(pool, run_id, next, &report, now).await?
    };
    if changed == 0 {
        // Lost a race to a retry (nothing to do) or the status cannot take a result.
        let run = load_checked(pool, run_id, &report.issue_id).await?;
        return if is_finished(&run) {
            Ok(Applied::Unchanged)
        } else {
            Err(CallbackError::Conflict(run.status))
        };
    }
    if next != Status::Queued {
        notify(pool, mailer, config, &run, &report, now).await;
    }
    Ok(Applied::Changed)
}

/// The status the report moves the run to, and the report as recorded (a
/// usage limit with no attempts left is a failure with a note).
fn decide(run: &RunView, report: &RunReport, config: &Config) -> (Status, RunReport) {
    match report.conclusion {
        Conclusion::Completed => (Status::Succeeded, report.clone()),
        Conclusion::Failed => (Status::Failed, report.clone()),
        Conclusion::UsageLimited if run.attempts < config.max_attempts => {
            (Status::Queued, report.clone())
        }
        Conclusion::UsageLimited => {
            let exhausted = RunReport {
                conclusion: Conclusion::Failed,
                notes: Some(OUT_OF_ATTEMPTS.into()),
                ..report.clone()
            };
            (Status::Failed, exhausted)
        }
    }
}

/// A result already recorded (or a row that never runs): a retried callback.
fn is_finished(run: &RunView) -> bool {
    matches!(
        run.status(),
        Some(
            Status::Succeeded
                | Status::Failed
                | Status::Skipped
                | Status::Duplicate
                | Status::Previewed
        )
    )
}

/// Back to the queue; the intake dispatches it again after the pause.
async fn requeue(pool: &MySqlPool, run_id: u64, now: DateTime<Utc>) -> Result<u64, sqlx::Error> {
    let retry_at = now + Duration::minutes(USAGE_LIMIT_RETRY_MINUTES);
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs SET status = 'queued', retry_at = ?, \
         last_error = 'usage limit', started_at = NULL WHERE id = ? AND {}",
        may_become(Status::Queued)
    )))
    .bind(retry_at)
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected())
}

async fn close(
    pool: &MySqlPool,
    run_id: u64,
    next: Status,
    report: &RunReport,
    now: DateTime<Utc>,
) -> Result<u64, CallbackError> {
    let agent = report.agent.as_ref();
    let result = serde_json::to_string(report)
        .map_err(|error| CallbackError::Invalid(format!("report is not JSON: {error}")))?;
    let last_error = (next == Status::Failed)
        .then(|| report.notes.as_deref().map(|notes| cut(notes, NOTES_MAX)))
        .flatten();
    Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE posthog_autofix_runs SET status = ?, outcome = ?, verification = ?, \
         target_repo = ?, pr_url = ?, github_run_url = COALESCE(?, github_run_url), \
         cost_usd = ?, last_error = ?, result = CAST(? AS JSON), finished_at = ? \
         WHERE id = ? AND {}",
        may_become(next)
    )))
    .bind(next.as_str())
    .bind(agent.map(|agent| agent.outcome.as_str()))
    .bind(report.verification.as_str())
    .bind(
        agent
            .and_then(|agent| agent.target_repo)
            .map(TargetRepo::as_str),
    )
    .bind(report.pr_url.as_deref())
    .bind(report.github_run_url.as_deref())
    .bind(report.cost_usd.map(|cost| (cost * 100.0).round() / 100.0))
    .bind(last_error)
    .bind(result)
    .bind(now)
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected())
}

/// Emails the developers; a failed email never fails the callback.
async fn notify(
    pool: &MySqlPool,
    mailer: &impl Mailer,
    config: &Config,
    run: &RunView,
    report: &RunReport,
    now: DateTime<Utc>,
) {
    let email = emails::run_finished(run, report, config);
    if !deliver(mailer, &config.recipients, &email).await {
        return;
    }
    let marked = sqlx::query("UPDATE posthog_autofix_runs SET notified_at = ? WHERE id = ?")
        .bind(now)
        .bind(run.id)
        .execute(pool)
        .await;
    if let Err(error) = marked {
        tracing::warn!(%error, run_id = run.id, "could not record notified_at");
    }
}

async fn load_checked(
    pool: &MySqlPool,
    run_id: u64,
    issue_id: &str,
) -> Result<RunView, CallbackError> {
    load_run(pool, run_id)
        .await?
        .filter(|run| run.posthog_issue_id == issue_id)
        .ok_or(CallbackError::NotFound)
}

fn validate_report(report: &RunReport) -> Result<(), CallbackError> {
    validate_issue_id(&report.issue_id)?;
    if let Some(url) = &report.github_run_url {
        validate_github_url("github_run_url", url)?;
    }
    if let Some(url) = &report.pr_url {
        validate_github_url("pr_url", url)?;
    }
    if report.conclusion == Conclusion::Completed && report.agent.is_none() {
        return Err(CallbackError::Invalid(
            "agent is required when conclusion is completed".into(),
        ));
    }
    if report
        .cost_usd
        .is_some_and(|cost| !(0.0..1_000_000.0).contains(&cost))
    {
        return Err(CallbackError::Invalid("cost_usd is out of range".into()));
    }
    Ok(())
}

fn validate_issue_id(id: &str) -> Result<(), CallbackError> {
    if is_issue_id(id) {
        Ok(())
    } else {
        Err(CallbackError::Invalid(
            "issue_id is not a lowercase UUID".into(),
        ))
    }
}

fn validate_github_url(field: &str, url: &str) -> Result<(), CallbackError> {
    if url.starts_with(GITHUB_PREFIX) && url.chars().count() <= URL_MAX {
        Ok(())
    } else {
        Err(CallbackError::Invalid(format!(
            "{field} must be a https://github.com/ link of at most {URL_MAX} characters"
        )))
    }
}

fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(secret: Option<&str>) -> Config {
        Config {
            callback_secret: secret.map(str::to_string),
            ..Config::default()
        }
    }

    #[test]
    fn only_the_exact_bearer_secret_is_authorized() {
        let config = config(Some("s3cret"));
        assert!(authorized(&config, Some("Bearer s3cret")));
        assert!(!authorized(&config, Some("s3cret")));
        assert!(!authorized(&config, Some("Bearer s3cret ")));
        assert!(!authorized(&config, Some("bearer s3cret")));
        assert!(!authorized(&config, Some("Bearer other")));
        assert!(!authorized(&config, None));
    }

    #[test]
    fn an_unset_secret_refuses_everyone() {
        assert!(!authorized(&config(None), Some("Bearer ")));
        assert!(!authorized(&config(None), None));
    }

    #[test]
    fn issue_ids_are_lowercase_uuids() {
        assert!(validate_issue_id("01a158ec-b8d5-7002-bb99-07e4e724acee").is_ok());
        assert!(validate_issue_id("01A158EC-b8d5-7002-bb99-07e4e724acee").is_err());
        assert!(validate_issue_id("01a158ecb8d570020bb9907e4e724acee0000").is_err());
        assert!(validate_issue_id("01a158ec-b8d5-7002-bb99-07e4e724acez").is_err());
        assert!(validate_issue_id("").is_err());
    }

    #[test]
    fn links_must_be_github_and_short() {
        assert!(validate_github_url("pr_url", "https://github.com/o/r/pull/1").is_ok());
        assert!(validate_github_url("pr_url", "https://evil.example/o/r").is_err());
        assert!(validate_github_url("pr_url", "http://github.com/o/r").is_err());
        let long = format!("{GITHUB_PREFIX}{}", "a".repeat(URL_MAX));
        assert!(validate_github_url("pr_url", &long).is_err());
    }
}
