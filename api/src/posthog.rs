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

/// The route template that served a request, as `/v1/customers/{id}`, kept in
/// the response extensions for `report_server_errors`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteTemplate(pub String);

/// The properties of a 5xx `$exception`, with the route template and build.
///
/// They let the auto-fix agent go from the event to the code. `lookup` reads
/// the Lambda runtime variables, so tests never touch the process environment.
pub fn server_error_properties<F>(
    method: &str,
    path: &str,
    route: Option<&str>,
    status: u16,
    detail: Option<&crate::error::ErrorDetail>,
    lookup: F,
) -> Value
where
    F: Fn(&str) -> Option<String>,
{
    let (kind, message) = detail.map_or_else(
        || ("HttpError", format!("{method} {path} answered {status}")),
        |detail| (detail.kind, detail.message.clone()),
    );
    json!({
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
        // The template, not the concrete path: null for an unmatched route.
        "api_route": route,
        "api_status": status,
        // The git short sha baked in at build time (`GIT_SHA`).
        "release": option_env!("GIT_SHA").unwrap_or("unknown"),
        "lambda_function": lookup("AWS_LAMBDA_FUNCTION_NAME"),
        "lambda_version": lookup("AWS_LAMBDA_FUNCTION_VERSION"),
    })
}

/// A request the API answered with a 5xx, as an `$exception` in Error
/// Tracking next to the browser's. `detail` is what failed, when known.
pub async fn capture_server_error(
    http: &reqwest::Client,
    method: &str,
    path: &str,
    route: Option<&str>,
    status: u16,
    detail: Option<&crate::error::ErrorDetail>,
) {
    let properties = server_error_properties(method, path, route, status, detail, |key| {
        std::env::var(key).ok()
    });
    capture(http, "$exception", "granite-api", properties).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lambda_env(key: &str) -> Option<String> {
        match key {
            "AWS_LAMBDA_FUNCTION_NAME" => Some("granite-api".into()),
            "AWS_LAMBDA_FUNCTION_VERSION" => Some("42".into()),
            _ => None,
        }
    }

    #[test]
    fn properties_carry_route_build_and_the_existing_fields() {
        let detail = crate::error::ErrorDetail {
            kind: "Panic",
            message: "boom".into(),
        };
        let props = server_error_properties(
            "GET",
            "/v1/customers/7",
            Some("/v1/customers/{id}"),
            500,
            Some(&detail),
            lambda_env,
        );
        assert_eq!(props["api_route"], "/v1/customers/{id}");
        assert_eq!(props["api_path"], "/v1/customers/7");
        assert_eq!(props["api_method"], "GET");
        assert_eq!(props["api_status"], 500);
        assert_eq!(props["error_source"], "api");
        assert_eq!(props["$process_person_profile"], false);
        assert_eq!(props["$exception_list"][0]["type"], "Panic");
        assert_eq!(props["$exception_list"][0]["value"], "boom");
        assert_eq!(props["lambda_function"], "granite-api");
        assert_eq!(props["lambda_version"], "42");
        assert_eq!(
            props["release"],
            option_env!("GIT_SHA").unwrap_or("unknown")
        );
    }

    #[test]
    fn missing_route_and_lambda_env_are_null() {
        let props = server_error_properties("GET", "/nope", None, 502, None, |_| None);
        assert!(props["api_route"].is_null());
        assert!(props["lambda_function"].is_null());
        assert!(props["lambda_version"].is_null());
        assert!(props["release"].is_string());
        assert_eq!(props["$exception_list"][0]["type"], "HttpError");
        assert_eq!(
            props["$exception_list"][0]["value"],
            "GET /nope answered 502"
        );
    }
}
