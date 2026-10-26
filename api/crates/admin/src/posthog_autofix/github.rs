//! Starting the fix workflow (`workflow_dispatch`).
//!
//! The dispatch carries only the ledger id, the PostHog issue id and the id
//! of the bug's latest event. The workflow reads everything else itself, so
//! error text (user and third-party input) never reaches a workflow
//! expression.

use super::config::Config;
use super::posthog::{is_event_uuid, is_issue_id};
use serde_json::{Value, json};

/// The inputs of one `posthog-autofix.yml` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatch<'a> {
    /// The ledger row (`posthog_autofix_runs.id`).
    pub run_id: u64,
    pub issue_id: &'a str,
    /// The bug's latest `$exception` event; `None` on rows of the issue intake.
    pub event_id: Option<&'a str>,
}

/// GitHub refused or could not be reached; the row is retried later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError(pub String);

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[allow(async_fn_in_trait)]
pub trait Dispatcher: Sync {
    async fn dispatch(&self, dispatch: &Dispatch<'_>) -> Result<(), DispatchError>;
}

/// The GitHub REST API.
pub struct GitHub<'a> {
    pub config: &'a Config,
    pub http: &'a reqwest::Client,
}

impl Dispatcher for GitHub<'_> {
    async fn dispatch(&self, dispatch: &Dispatch<'_>) -> Result<(), DispatchError> {
        let token = self
            .config
            .github_token
            .as_deref()
            .ok_or_else(|| DispatchError("POSTHOG_AUTOFIX_GITHUB_TOKEN is not set".into()))?;
        let (url, body) = dispatch_request(self.config, dispatch)?;
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                DispatchError(format!("GitHub unreachable: {}", error.without_url()))
            })?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        let excerpt: String = body.chars().take(BODY_EXCERPT).collect();
        Err(DispatchError(format!(
            "GitHub dispatch failed: {status} {excerpt}"
        )))
    }
}

/// Characters of an error response kept in a [`DispatchError`].
const BODY_EXCERPT: usize = 200;

/// The `workflow_dispatch` endpoint and body. Inputs are strings: GitHub
/// rejects other JSON types, so a row without an event sends `""`. Ids that
/// are not lowercase UUIDs never leave the api.
fn dispatch_request(
    config: &Config,
    dispatch: &Dispatch<'_>,
) -> Result<(String, Value), DispatchError> {
    if !is_issue_id(dispatch.issue_id) {
        return Err(DispatchError("issue_id is not a lowercase UUID".into()));
    }
    let event_id = dispatch.event_id.unwrap_or_default();
    if !event_id.is_empty() && !is_event_uuid(event_id) {
        return Err(DispatchError("event_id is not a lowercase UUID".into()));
    }
    let url = format!(
        "https://api.github.com/repos/{}/actions/workflows/{}/dispatches",
        config.github_repo, config.github_workflow
    );
    let body = json!({
        "ref": config.github_ref,
        "inputs": {
            "run_id": dispatch.run_id.to_string(),
            "issue_id": dispatch.issue_id,
            "event_id": event_id,
        },
    });
    Ok((url, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dispatch_starts_the_workflow_with_the_run_issue_and_event_ids() {
        let dispatch = Dispatch {
            run_id: 42,
            issue_id: "01a158ec-b8d5-7002-bb99-07e4e724acee",
            event_id: Some("01a17880-7a7a-740b-ac7c-30975f48657a"),
        };
        let (url, body) = dispatch_request(&Config::default(), &dispatch).unwrap();
        assert_eq!(
            url,
            "https://api.github.com/repos/DmitryZaika/granite-webhooks/actions/workflows/posthog-autofix.yml/dispatches"
        );
        assert_eq!(
            body,
            json!({
                "ref": "main",
                "inputs": {
                    "run_id": "42",
                    "issue_id": "01a158ec-b8d5-7002-bb99-07e4e724acee",
                    "event_id": "01a17880-7a7a-740b-ac7c-30975f48657a",
                },
            })
        );
        let without_event = Dispatch {
            event_id: None,
            ..dispatch
        };
        let (_, body) = dispatch_request(&Config::default(), &without_event).unwrap();
        assert_eq!(body["inputs"]["event_id"], "");
    }

    #[test]
    fn ids_that_are_not_uuids_never_leave_the_api() {
        let bad_event = Dispatch {
            run_id: 1,
            issue_id: "01a158ec-b8d5-7002-bb99-07e4e724acee",
            event_id: Some("x' || true"),
        };
        assert!(dispatch_request(&Config::default(), &bad_event).is_err());
        let bad_issue = Dispatch {
            issue_id: "01A158EC-b8d5-7002-bb99-07e4e724acee",
            event_id: None,
            ..bad_event
        };
        assert!(dispatch_request(&Config::default(), &bad_issue).is_err());
    }
}
