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

/// A request the API answered with a 5xx, as an `$exception` in Error
/// Tracking next to the browser's. `detail` is what failed, when known.
pub async fn capture_server_error(
    http: &reqwest::Client,
    method: &str,
    path: &str,
    status: u16,
    detail: Option<&crate::error::ErrorDetail>,
) {
    let (kind, message) = detail.map_or_else(
        || ("HttpError", format!("{method} {path} answered {status}")),
        |detail| (detail.kind, detail.message.clone()),
    );
    let properties = json!({
        "$exception_list": [{
            "type": kind,
            "value": message,
            "mechanism": { "handled": true, "synthetic": false },
        }],
        // One shared id for the server: no person profile for it.
        "$process_person_profile": false,
        "error_source": "api",
        "api_method": method,
        "api_path": path,
        "api_status": status,
    });
    capture(http, "$exception", "granite-api", properties).await;
}
