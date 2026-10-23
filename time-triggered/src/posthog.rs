//! Reports failures of the scheduler to `PostHog` Error Tracking, where the
//! auto-fix pipeline picks them up. Reporting never fails the tick.

use lambda_runtime::tracing;
use serde_json::{json, Value};
use std::fmt::Display;
use std::time::Duration;

const API_ENDPOINT: &str = "https://us.i.posthog.com/i/v0/e/";
const DISTINCT_ID: &str = "server-time-triggered";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MESSAGE_CHARS: usize = 1000;

/// Replaces anything that looks like an email address, so a provider error
/// that echoes a recipient never leaves the Lambda.
fn redact(message: &str) -> String {
    let redacted = message
        .split_inclusive(char::is_whitespace)
        .map(|word| {
            if word.contains('@') {
                let trailing = &word[word.trim_end().len()..];
                format!("[redacted]{trailing}")
            } else {
                word.to_string()
            }
        })
        .collect::<String>();
    redacted.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// The `$exception` event for one failed job. `lookup` reads the environment
/// and `release` is the build's git sha, so tests need neither.
fn exception_event<F>(
    api_key: &str,
    job: &str,
    message: &str,
    release: Option<&str>,
    lookup: F,
) -> Value
where
    F: Fn(&str) -> Option<String>,
{
    json!({
        "api_key": api_key,
        "event": "$exception",
        "distinct_id": DISTINCT_ID,
        "properties": {
            "$exception_list": [{
                "type": "JobError",
                "value": format!("{job}: {}", redact(message)),
                "mechanism": {"handled": true, "synthetic": false},
            }],
            "$process_person_profile": false,
            "error_source": "time-triggered",
            "job": job,
            "release": release.unwrap_or("unknown"),
            "lambda_function": lookup("AWS_LAMBDA_FUNCTION_NAME")
                .unwrap_or_else(|| "local".to_string()),
            "lambda_version": lookup("AWS_LAMBDA_FUNCTION_VERSION")
                .unwrap_or_else(|| "local".to_string()),
        },
    })
}

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Sends one `$exception` event for a failed `job`. Does nothing without
/// `POSTHOG_API_KEY`; a `PostHog` failure is only logged.
pub async fn report_failure(job: &str, error: &(impl Display + ?Sized)) {
    let Some(api_key) = env_lookup("POSTHOG_API_KEY").filter(|key| !key.trim().is_empty()) else {
        return;
    };
    let event = exception_event(
        &api_key,
        job,
        &error.to_string(),
        option_env!("GIT_SHA"),
        env_lookup,
    );
    let sent = async {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?
            .post(API_ENDPOINT)
            .json(&event)
            .send()
            .await?
            .error_for_status()
    }
    .await;
    if let Err(report_error) = sent {
        tracing::warn!(%report_error, job, "Failed to report job failure to PostHog");
    }
}

/// Reports `result`'s error (once, here) and hands the result back, so a step
/// can report and still propagate with `?`.
pub async fn reported<T, E: Display>(job: &str, result: Result<T, E>) -> Result<T, E> {
    if let Err(error) = &result {
        report_failure(job, error).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(key: &str) -> Option<String> {
        match key {
            "AWS_LAMBDA_FUNCTION_NAME" => Some("time-triggered".into()),
            "AWS_LAMBDA_FUNCTION_VERSION" => Some("42".into()),
            _ => None,
        }
    }

    #[test]
    fn event_carries_the_exception_and_build_info() {
        let event = exception_event("key", "call-points", "boom", Some("abc123"), lookup);
        assert_eq!(event["api_key"], "key");
        assert_eq!(event["event"], "$exception");
        assert_eq!(event["distinct_id"], "server-time-triggered");
        let props = &event["properties"];
        let item = &props["$exception_list"][0];
        assert_eq!(item["type"], "JobError");
        assert_eq!(item["value"], "call-points: boom");
        assert_eq!(item["mechanism"]["handled"], true);
        assert_eq!(item["mechanism"]["synthetic"], false);
        assert_eq!(props["$process_person_profile"], false);
        assert_eq!(props["error_source"], "time-triggered");
        assert_eq!(props["job"], "call-points");
        assert_eq!(props["release"], "abc123");
        assert_eq!(props["lambda_function"], "time-triggered");
        assert_eq!(props["lambda_version"], "42");
    }

    #[test]
    fn release_and_lambda_info_fall_back() {
        let event = exception_event("key", "job", "boom", None, |_| None);
        let props = &event["properties"];
        assert_eq!(props["release"], "unknown");
        assert_eq!(props["lambda_function"], "local");
        assert_eq!(props["lambda_version"], "local");
    }

    #[test]
    fn email_addresses_are_redacted_and_long_messages_clipped() {
        let event = exception_event(
            "key",
            "scheduled-emails",
            "Email address is not verified: jane@example.com. Retry\nlater",
            None,
            lookup,
        );
        let value = event["properties"]["$exception_list"][0]["value"]
            .as_str()
            .unwrap_or_default();
        assert!(!value.contains("jane@"), "{value}");
        assert!(value.contains("[redacted] Retry"), "{value}");
        let long = redact(&"x".repeat(5000));
        assert_eq!(long.chars().count(), MAX_MESSAGE_CHARS);
    }

    #[tokio::test]
    async fn reporting_without_a_key_does_nothing_and_keeps_the_result() {
        // POSTHOG_API_KEY is unset in the test environment.
        let result: Result<u8, String> = Err("nope".into());
        assert_eq!(reported("job", result).await, Err("nope".to_string()));
        assert_eq!(reported::<_, String>("job", Ok(3)).await, Ok(3));
    }
}
