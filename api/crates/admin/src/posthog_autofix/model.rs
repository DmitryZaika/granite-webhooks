//! The ledger's vocabulary and the workflow's callback bodies.
//!
//! Every enum here is stored in `posthog_autofix_runs` or sent by the
//! workflow, so its `as_str` spelling is the column's ENUM value and its serde
//! name; changing one means a migration and a workflow change.

use crate::openapi::ToSchema;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a run is in its life. See the migration for the state diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Discovered,
    Queued,
    Dispatched,
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Skipped,
    Duplicate,
    Previewed,
}

impl Status {
    pub const ALL: [Self; 10] = [
        Self::Discovered,
        Self::Queued,
        Self::Dispatched,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::TimedOut,
        Self::Skipped,
        Self::Duplicate,
        Self::Previewed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Queued => "queued",
            Self::Dispatched => "dispatched",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Skipped => "skipped",
            Self::Duplicate => "duplicate",
            Self::Previewed => "previewed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }

    /// A workflow run exists for the row and has not reported back.
    pub const fn is_in_flight(self) -> bool {
        matches!(self, Self::Dispatched | Self::Running)
    }

    /// Whether the ledger may move a row from `self` to `next`.
    ///
    /// `TimedOut` still takes a late result once, so a fix that finished after
    /// the watchdog gave up is recorded and emailed instead of lost.
    pub const fn can_become(self, next: Self) -> bool {
        use Status::{
            Discovered, Dispatched, Duplicate, Failed, Previewed, Queued, Running, Skipped,
            Succeeded, TimedOut,
        };
        matches!(
            (self, next),
            (Discovered, Queued | Skipped | Duplicate)
                | (Queued, Dispatched | Previewed | Failed | Skipped)
                | (Dispatched, Running | Queued | Failed | TimedOut)
                | (Running, Succeeded | Failed | TimedOut | Queued)
                | (TimedOut, Succeeded | Failed)
        )
    }

    /// The statuses a row may leave to become `next`.
    pub fn sources_of(next: Self) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|from| from.can_become(next))
            .collect()
    }
}

/// Why a row was skipped (`skip_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Resolved in PostHog before it settled.
    Resolved,
    /// Suppressed in PostHog: its events are dropped.
    Suppressed,
    /// Matched `POSTHOG_AUTOFIX_EXCLUDE` (network noise, browser extensions).
    Excluded,
    /// Raised by the removed Remix server (`posthog-node`).
    LegacyRuntime,
    /// Raised outside production (localhost, a dev host).
    NonProduction,
    /// PostHog no longer has the issue of the bug's events (deleted or merged).
    NoEvents,
    /// A browser toast that tells the user what to fix, not a failure.
    HintToast,
    /// A browser's generic copy of an API 500 that the api reports itself.
    Symptom,
    /// An API 4xx that enforced a business rule (`{"error":"<sentence>"}`); a
    /// 403 is never one.
    BusinessRule4xx,
}

impl SkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Suppressed => "suppressed",
            Self::Excluded => "excluded",
            Self::LegacyRuntime => "legacy_runtime",
            Self::NonProduction => "non_production",
            Self::NoEvents => "no_events",
            Self::HintToast => "hint_toast",
            Self::Symptom => "symptom",
            Self::BusinessRule4xx => "business_rule_4xx",
        }
    }
}

/// Which code raised the error, from the event's `distinct_id` and `$lib`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum Source {
    /// The browser SPA (`$lib` = `web`).
    #[serde(rename = "web")]
    Web,
    /// The `api` Lambda (`distinct_id` = `granite-api`).
    #[serde(rename = "granite-api")]
    Api,
    /// The `webhooks` Lambda (`distinct_id` = `server-webhooks`).
    #[serde(rename = "server-webhooks")]
    Webhooks,
    /// The `time-triggered` scheduler Lambda (`distinct_id` = `server-time-triggered`).
    #[serde(rename = "server-time-triggered")]
    TimeTriggered,
    /// The Remix server removed on 2026-10-13 (`$lib` = `posthog-node`).
    #[serde(rename = "legacy-node")]
    LegacyNode,
    #[serde(rename = "unknown")]
    Unknown,
}

impl Source {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Api => "granite-api",
            Self::Webhooks => "server-webhooks",
            Self::TimeTriggered => "server-time-triggered",
            Self::LegacyNode => "legacy-node",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::Web,
            Self::Api,
            Self::Webhooks,
            Self::TimeTriggered,
            Self::LegacyNode,
            Self::Unknown,
        ]
        .into_iter()
        .find(|source| source.as_str() == value)
    }

    /// The name the emails show.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Web => "web app",
            Self::Api => "API",
            Self::Webhooks => "webhooks",
            Self::TimeTriggered => "scheduled jobs",
            Self::LegacyNode => "old Remix server",
            Self::Unknown => "unknown",
        }
    }
}

/// What the agent concluded about the issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Fixed, with a test that failed before the fix and passes after it.
    FixedWithTest,
    /// Fixed, but no automated test could reproduce it; a person tests it.
    FixedWithoutTest,
    /// Configuration, operations, a missing migration, a third party or
    /// network noise: nothing to change in code.
    NotACodeBug,
    /// The reported behaviour could not be found in the code.
    NotReproducible,
    /// An open fix or a recent commit already covers it.
    Duplicate,
    /// Too large, risky, cross-repo, or touching auth, billing or migrations.
    NeedsHuman,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FixedWithTest => "fixed_with_test",
            Self::FixedWithoutTest => "fixed_without_test",
            Self::NotACodeBug => "not_a_code_bug",
            Self::NotReproducible => "not_reproducible",
            Self::Duplicate => "duplicate",
            Self::NeedsHuman => "needs_human",
        }
    }

    /// Words for email subjects, e.g. `[AI fix] Fixed with a test: TypeError`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::FixedWithTest => "Fixed with a test",
            Self::FixedWithoutTest => "Fixed, needs a manual test",
            Self::NotACodeBug => "Not a code bug",
            Self::NotReproducible => "Not reproducible",
            Self::Duplicate => "Already covered",
            Self::NeedsHuman => "Needs a human",
        }
    }

    /// Only a fix produces commits, and so a pull request.
    pub const fn is_fix(self) -> bool {
        matches!(self, Self::FixedWithTest | Self::FixedWithoutTest)
    }
}

/// The repository a run changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum TargetRepo {
    #[serde(rename = "granite-webhooks")]
    GraniteWebhooks,
    #[serde(rename = "general_datebase")]
    GeneralDatebase,
}

impl TargetRepo {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GraniteWebhooks => "granite-webhooks",
            Self::GeneralDatebase => "general_datebase",
        }
    }
}

/// What the workflow proved about the agent's commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// The test failed on the test commit, passed on the fix commit, and the
    /// repo's checks passed: the pull request is ready for review.
    Verified,
    /// A check failed: the pull request is a draft with the evidence.
    Unverified,
    /// No commits (no fix, or a fix without a test).
    NotApplicable,
}

impl Verification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
            Self::NotApplicable => "not_applicable",
        }
    }
}

/// How the workflow run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// The agent returned a result (which may be "no code fix").
    Completed,
    /// The run broke (setup, the agent erroring, the push failing).
    Failed,
    /// The Claude subscription's usage limit stopped the agent: the run is
    /// queued again and retried later.
    UsageLimited,
}

/// Whether the agent wrote a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// A test that reproduces the bug.
    Written,
    /// No automated test could reproduce it (`why_not_possible` says why).
    NotPossible,
    /// No fix, so no test.
    NotApplicable,
}

/// The agent's test, as it reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TestReport {
    /// Whether a test was written.
    pub status: TestStatus,
    /// Test files, relative to the target repository.
    #[serde(default)]
    pub files: Vec<String>,
    /// The command that runs it, e.g. `cargo test -p api --test sms tts_reply`.
    pub command: Option<String>,
    /// Why no automated test was possible.
    pub why_not_possible: Option<String>,
    /// How a person checks the fix by hand.
    pub manual_test_steps: Option<String>,
}

/// The agent's structured result (`.github/posthog-autofix/result.schema.json`).
/// It is the agent's claim; [`RunReport::verification`] is what the workflow
/// proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentResult {
    /// What the agent concluded.
    pub outcome: Outcome,
    /// The repository it changed, if any.
    pub target_repo: Option<TargetRepo>,
    /// One short sentence: what goes wrong and on which route (the title when
    /// there is no `pr_title`, and the Issue line of an older report).
    pub summary: String,
    /// The bug report's Given: the state the bug needs, e.g. `a company with
    /// calling turned off`. Empty from a workflow older than these fields.
    #[serde(default)]
    pub given: String,
    /// When: the user's action, with the page and the API route.
    #[serde(default)]
    pub when: String,
    /// Then: what the system does at that point, e.g. `the API answers 403
    /// calling_disabled`.
    #[serde(default)]
    pub then: String,
    /// What goes wrong now.
    #[serde(default)]
    pub actual: String,
    /// What should happen instead.
    #[serde(default)]
    pub expected: String,
    /// What users notice, in one sentence; the email adds the counts.
    #[serde(default)]
    pub impact: String,
    /// How a developer sees the fix by hand, in one line; empty without a fix.
    #[serde(default)]
    pub check: String,
    /// Why it happened, in a few sentences.
    pub root_cause: String,
    /// One or two short sentences: what the fix changes, naming the function
    /// and file (the Fix line); empty without a fix.
    pub fix_summary: String,
    /// What the fix could break, and how likely.
    pub risk: String,
    /// The test, or why there is none.
    pub test: TestReport,
    /// The next step for a person (a suppression rule, a migration to apply).
    pub recommendation: Option<String>,
    /// The pull request or commit that already covers the issue.
    pub duplicate_of: Option<String>,
    /// The pull request title.
    pub pr_title: Option<String>,
}

/// Body of `POST /v1/posthog-autofix/runs/{id}/started`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StartedReport {
    /// The run's PostHog issue id, checked against the ledger row.
    pub issue_id: String,
    /// The GitHub Actions run, e.g. `https://github.com/<owner>/<repo>/actions/runs/<id>`.
    pub github_run_url: String,
}

/// Body of `POST /v1/posthog-autofix/runs/{id}/result`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunReport {
    /// The run's PostHog issue id, checked against the ledger row.
    pub issue_id: String,
    /// How the workflow run ended.
    pub conclusion: Conclusion,
    /// The agent's result; absent when the run broke before it answered.
    pub agent: Option<AgentResult>,
    /// What the workflow proved about the commits.
    pub verification: Verification,
    /// Why verification failed, or what broke the run.
    pub notes: Option<String>,
    /// The pull request, when one was opened.
    pub pr_url: Option<String>,
    /// The pull request is a draft (unverified).
    #[serde(default)]
    pub pr_draft: bool,
    /// The agent's cost as Claude Code estimated it, in US dollars.
    pub cost_usd: Option<f64>,
    /// The GitHub Actions run.
    pub github_run_url: Option<String>,
}

/// A ledger row as emails and callbacks read it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunView {
    pub id: u64,
    pub posthog_issue_id: String,
    /// The bug's latest `$exception` event; `None` on rows of the issue intake.
    pub event_uuid: Option<String>,
    pub issue_created_at: DateTime<Utc>,
    pub issue_name: String,
    pub issue_description: String,
    pub source: Option<String>,
    pub occurrences: Option<u32>,
    pub users: Option<u32>,
    pub signature: Option<String>,
    pub status: String,
    pub outcome: Option<String>,
    pub target_repo: Option<String>,
    pub pr_url: Option<String>,
    pub github_run_url: Option<String>,
    pub attempts: u8,
    pub dispatched_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl RunView {
    /// The columns [`RunView`] reads, for `SELECT {RUN_VIEW_COLUMNS} FROM posthog_autofix_runs`.
    pub const COLUMNS: &'static str = "id, posthog_issue_id, event_uuid, issue_created_at, issue_name, \
        issue_description, CAST(source AS CHAR) AS source, occurrences, users, signature, \
        CAST(status AS CHAR) AS status, CAST(outcome AS CHAR) AS outcome, \
        CAST(target_repo AS CHAR) AS target_repo, pr_url, github_run_url, attempts, \
        dispatched_at, started_at, finished_at";

    pub fn status(&self) -> Option<Status> {
        Status::parse(&self.status)
    }
}

/// Where and to whom an issue's error last happened, for the dry-run email.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LastSeen {
    /// The page, for a browser error.
    pub url: Option<String>,
    /// The request, as `GET /v1/customers/7`, for an API error.
    pub request: Option<String>,
    /// The signed-in user (the browser's `distinct_id`).
    pub user_id: Option<i32>,
    /// That user's name and company, from `users`.
    pub user_name: Option<String>,
    pub company_id: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip() {
        for status in Status::ALL {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        assert_eq!(Status::parse("nope"), None);
    }

    #[test]
    fn terminal_states_only_take_a_late_result() {
        for terminal in [
            Status::Succeeded,
            Status::Failed,
            Status::Skipped,
            Status::Duplicate,
            Status::Previewed,
        ] {
            assert!(
                Status::ALL.iter().all(|next| !terminal.can_become(*next)),
                "{terminal:?} must be terminal"
            );
        }
        assert!(Status::TimedOut.can_become(Status::Succeeded));
        assert!(!Status::TimedOut.can_become(Status::Running));
    }

    #[test]
    fn a_result_closes_only_a_run_that_was_started_or_timed_out() {
        assert_eq!(
            Status::sources_of(Status::Succeeded),
            vec![Status::Running, Status::TimedOut]
        );
        assert_eq!(
            Status::sources_of(Status::Running),
            vec![Status::Dispatched]
        );
    }

    #[test]
    fn wire_names_match_the_ledger_columns() {
        assert_eq!(
            serde_json::to_value(Source::Api).unwrap(),
            serde_json::json!("granite-api")
        );
        assert_eq!(
            serde_json::to_value(TargetRepo::GeneralDatebase).unwrap(),
            serde_json::json!("general_datebase")
        );
        assert_eq!(
            serde_json::to_value(Outcome::NotACodeBug).unwrap(),
            serde_json::json!(Outcome::NotACodeBug.as_str())
        );
        assert_eq!(
            serde_json::to_value(Verification::NotApplicable).unwrap(),
            serde_json::json!(Verification::NotApplicable.as_str())
        );
    }
}
