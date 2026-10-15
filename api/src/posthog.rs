//! Server events to `PostHog`, sent only where server reporting is on
//! (`POSTHOG_SERVER_ENABLED=true` and `POSTHOG_KEY`, like the CRM's
//! `posthog.server.ts`). A failed send is only traced.

use serde_json::{Value, json};

/// Captures `event` for `distinct_id`.
pub async fn capture(http: &reqwest::Client, event: &str, distinct_id: &str, properties: Value) {
    if std::env::var("POSTHOG_SERVER_ENABLED").ok().as_deref() != Some("true") {
        return;
    }
    let Some(key) = std::env::var("POSTHOG_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
    else {
        return;
    };
    let host = std::env::var("POSTHOG_HOST").unwrap_or_else(|_| "https://us.i.posthog.com".into());
    let body = json!({ "api_key": key, "event": event, "distinct_id": distinct_id, "properties": properties });
    let result = http
        .post(format!("{}/capture/", host.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .json(&body)
        .send()
        .await;
    if let Err(err) = result {
        lambda_http::tracing::warn!(error = %err.without_url(), event, "PostHog event not sent");
    }
}
