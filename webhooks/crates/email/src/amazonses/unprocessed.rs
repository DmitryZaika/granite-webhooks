//! Reporting inbound emails the Lambda gives up on (answered 200 so
//! `EventBridge` stops retrying, but never stored in the CRM).

use std::collections::HashMap;
use std::time::Duration;

use lambda_http::tracing;

use crate::posthog::{PostHogEvent, client};

pub const UNPROCESSED_EMAIL_EVENT: &str = "inbound_email_unprocessed";

/// Kept well under `EventBridge`'s 5 s delivery timeout: a slow `PostHog` must
/// not turn a handled email into a retried one.
const REPORT_TIMEOUT: Duration = Duration::from_secs(3);

pub trait UnprocessedEmailReporter: Send + Sync {
    /// Record that the S3 object `bucket`/`key` will not be processed.
    /// Never pass addresses, subjects or bodies.
    fn report<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        error_kind: &'static str,
    ) -> impl Future<Output = ()> + Send + 'a;
}

/// Sends one `inbound_email_unprocessed` event to `PostHog`.
pub struct PostHogUnprocessedEmailReporter;

impl UnprocessedEmailReporter for PostHogUnprocessedEmailReporter {
    async fn report(&self, bucket: &str, key: &str, error_kind: &'static str) {
        let Ok(api_key) = std::env::var("POSTHOG_API_KEY") else {
            tracing::warn!("POSTHOG_API_KEY not set; unprocessed email not reported");
            return;
        };
        let event = PostHogEvent::new_named_event(
            api_key,
            UNPROCESSED_EMAIL_EVENT,
            unprocessed_email_properties(bucket, key, error_kind),
        );
        let posthog = client().await;
        match tokio::time::timeout(REPORT_TIMEOUT, posthog.capture(event)).await {
            Ok(Ok(response)) if response.status().is_success() => {}
            Ok(Ok(response)) => tracing::warn!(
                status = response.status().as_u16(),
                "PostHog rejected the unprocessed email event"
            ),
            Ok(Err(error)) => {
                tracing::warn!(%error, "Failed to send the unprocessed email event");
            }
            Err(_) => tracing::warn!("Timed out sending the unprocessed email event"),
        }
    }
}

fn unprocessed_email_properties(
    bucket: &str,
    key: &str,
    error_kind: &str,
) -> HashMap<String, serde_json::Value> {
    HashMap::from([
        ("bucket".to_string(), bucket.into()),
        ("object_key".to_string(), key.into()),
        ("error_kind".to_string(), error_kind.into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_carries_only_bucket_key_and_kind_without_a_person_profile() {
        let event = PostHogEvent::new_named_event(
            "key",
            UNPROCESSED_EMAIL_EVENT,
            unprocessed_email_properties(
                "granite-ses-inbound-emails",
                "abc123",
                "missing_receiver",
            ),
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event"], "inbound_email_unprocessed");
        assert_eq!(json["distinct_id"], "server-webhooks");
        let props = &json["properties"];
        assert_eq!(props["bucket"], "granite-ses-inbound-emails");
        assert_eq!(props["object_key"], "abc123");
        assert_eq!(props["error_kind"], "missing_receiver");
        assert_eq!(props["$process_person_profile"], false);
        assert!(props.get("$exception_list").is_none());
        assert!(props.get("$exception_fingerprint").is_none());
        assert!(props.get("lambda_function").is_some());
    }
}
