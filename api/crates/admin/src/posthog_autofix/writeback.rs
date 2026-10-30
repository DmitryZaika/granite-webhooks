//! Writing back to PostHog.
//!
//! Once a fix's pull request is open, its issue is assigned to whoever
//! reviews AI fixes, so the issue list shows it has an owner. PostHog takes
//! one assignee per issue, so several reviewers take turns by run. Off unless
//! `POSTHOG_AUTOFIX_WRITE_KEY` (`error_tracking:write`) and
//! `POSTHOG_AUTOFIX_ASSIGNEE` are both set.

use super::config::Config;
use serde_json::{Value, json};

/// Who PostHog issues of opened fixes are assigned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assignee {
    /// A PostHog user, by numeric id.
    User(u64),
    /// A PostHog role, by UUID.
    Role(String),
}

impl Assignee {
    /// `user:<numeric id>` or `role:<uuid>`.
    pub fn parse(value: &str) -> Option<Self> {
        let (kind, id) = value.trim().split_once(':')?;
        let id = id.trim();
        match kind.trim() {
            "user" => id.parse().ok().filter(|id| *id > 0).map(Self::User),
            "role" if is_uuid(id) => Some(Self::Role(id.to_ascii_lowercase())),
            _ => None,
        }
    }

    fn body(&self) -> Value {
        match self {
            Self::User(id) => json!({ "assignee": { "type": "user", "id": id } }),
            Self::Role(id) => json!({ "assignee": { "type": "role", "id": id } }),
        }
    }
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[allow(async_fn_in_trait)]
pub trait IssueWriter: Sync {
    /// Whether write-back is configured at all.
    fn enabled(&self) -> bool;
    /// Assigns the issue of run `run_id`; an error is logged and retried on
    /// the next poll.
    async fn assign(&self, run_id: u64, issue_id: &str) -> Result<(), String>;
}

/// The PostHog REST API.
pub struct PostHogWriter<'a> {
    pub config: &'a Config,
    pub http: &'a reqwest::Client,
}

impl IssueWriter for PostHogWriter<'_> {
    fn enabled(&self) -> bool {
        self.config.write_key.is_some() && !self.config.assignees.is_empty()
    }

    async fn assign(&self, run_id: u64, issue_id: &str) -> Result<(), String> {
        let (Some(key), Some(assignee)) = (
            &self.config.write_key,
            assignee_for(&self.config.assignees, run_id),
        ) else {
            return Err("write-back is not configured".into());
        };
        let (url, body) = assign_request(self.config, issue_id, assignee);
        // Assigning the same reviewer again changes nothing: safe to resend.
        let request = self.http.patch(url).bearer_auth(key).json(&body);
        let response = common::http::send_retrying(request)
            .await
            .map_err(|error| format!("PostHog unreachable: {}", error.without_url()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(200).collect();
        Err(format!("PostHog assign failed: {status} {excerpt}"))
    }
}

/// The reviewer whose turn run `run_id` is.
pub fn assignee_for(assignees: &[Assignee], run_id: u64) -> Option<&Assignee> {
    let count = u64::try_from(assignees.len())
        .ok()
        .filter(|count| *count > 0)?;
    assignees.get(usize::try_from(run_id % count).ok()?)
}

/// `PATCH /api/projects/{project}/error_tracking/issues/{id}/assign/`, from
/// PostHog's published API schema.
fn assign_request(config: &Config, issue_id: &str, assignee: &Assignee) -> (String, Value) {
    let url = format!(
        "{}/api/projects/{}/error_tracking/issues/{issue_id}/assign/",
        config.posthog_host, config.posthog_project_id
    );
    (url, assignee.body())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assignees_parse_users_and_roles_only() {
        assert_eq!(
            Assignee::parse("user:378526"),
            Some(Assignee::User(378_526))
        );
        assert_eq!(
            Assignee::parse(" role:0195D9F7-251F-0000-9A6B-CA0C49312D5C "),
            Some(Assignee::Role(
                "0195d9f7-251f-0000-9a6b-ca0c49312d5c".into()
            ))
        );
        for bad in [
            "",
            "user:",
            "user:0",
            "user:abc",
            "role:nope",
            "team:1",
            "378526",
        ] {
            assert_eq!(Assignee::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn reviewers_take_turns_by_run() {
        let two = [Assignee::User(1), Assignee::User(2)];
        assert_eq!(assignee_for(&two, 10), Some(&Assignee::User(1)));
        assert_eq!(assignee_for(&two, 11), Some(&Assignee::User(2)));
        assert_eq!(assignee_for(&two[..1], 11), Some(&Assignee::User(1)));
        assert_eq!(assignee_for(&[], 11), None);
    }

    #[test]
    fn the_request_matches_the_posthog_schema() {
        let (url, body) = assign_request(
            &Config::default(),
            "01a158ec-b8d5-7002-bb99-07e4e724acee",
            &Assignee::User(42),
        );
        assert_eq!(
            url,
            "https://us.posthog.com/api/projects/141685/error_tracking/issues/01a158ec-b8d5-7002-bb99-07e4e724acee/assign/"
        );
        assert_eq!(body, json!({ "assignee": { "type": "user", "id": 42 } }));
        let (_, role) = assign_request(&Config::default(), "x", &Assignee::Role("r".into()));
        assert_eq!(role, json!({ "assignee": { "type": "role", "id": "r" } }));
    }
}
