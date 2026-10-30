use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use axum::{
    body::Body,
    extract::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use lambda_http::request::RequestContext;
use lambda_http::tracing;

use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;
use crate::posthog::{PostHogEvent, client};
use serde_json::Value;
use std::collections::BTreeMap;

// middleware that shows how to consume the request body upfront
pub async fn print_request_body(
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, BasicResponse> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();
    let request = log_request_body(request, &uri).await?;
    tracing::info!(
        "Incoming request: {} {} (routing path: {})",
        method,
        uri,
        path
    );

    let response = next.run(request).await;

    if !response.status().is_success() {
        return log_response_body(response, &uri).await;
    }

    Ok(response)
}

async fn log_request_body(request: Request, uri: &Uri) -> Result<Request, BasicResponse> {
    let (parts, body) = request.into_parts();

    // This won't work if the body is a long-running stream
    let bytes = body
        .collect()
        .await
        .map_err(|_| internal_error("Middleware could not parse request body"))?
        .to_bytes();

    // Only the shape of a body is logged, never its values: bodies carry
    // customer names, phone numbers, emails and message text (PII).
    let body_schema = body_schema(&bytes);
    if uri.path() == "/cloudtalk/sms" || uri.path().starts_with("/cloudtalk/sms/") {
        // Empty rejected posts have no phone number. The Lambda source IP and user agent
        // are what identify the caller. Do not log the Authorization header.
        let (source_ip, user_agent) = sms_caller(&parts);
        if let Some(body_schema) = body_schema {
            tracing::info!(
                body_schema = ?body_schema,
                path = %uri.path(),
                source_ip = %source_ip,
                user_agent = %user_agent,
            );
        } else {
            // Fallback if the body isn't a valid JSON object (e.g., empty or plain text)
            tracing::info!(
                body_size = bytes.len(),
                path = %uri.path(),
                source_ip = %source_ip,
                user_agent = %user_agent,
                note = "Body is not a valid JSON object"
            );
        }
    } else if let Some(body_schema) = body_schema {
        tracing::info!(body_schema = ?body_schema, body_size = bytes.len(), path = %uri.path());
    } else {
        tracing::info!(body_size = bytes.len(), path = %uri.path());
    }

    Ok(Request::from_parts(parts, Body::from(bytes)))
}

/// Each top-level key of a JSON object body and the type of its value (lists
/// with their length), or None when the body is not a JSON object.
fn body_schema(bytes: &Bytes) -> Option<BTreeMap<String, String>> {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
        return None;
    };
    Some(
        map.iter()
            .map(|(key, value)| {
                let type_name = match value {
                    Value::Null => "Null".to_string(),
                    Value::Bool(_) => "Bool".to_string(),
                    Value::Number(_) => "Number".to_string(),
                    Value::String(_) => "String".to_string(),
                    Value::Array(items) => format!("Array({})", items.len()),
                    Value::Object(_) => "Object".to_string(),
                };
                (key.clone(), type_name)
            })
            .collect(),
    )
}

fn sms_caller(parts: &axum::http::request::Parts) -> (String, String) {
    if let Some(RequestContext::ApiGatewayV2(context)) = parts.extensions.get::<RequestContext>() {
        let source_ip = context.http.source_ip.clone().unwrap_or_default();
        let user_agent = context
            .http
            .user_agent
            .clone()
            .filter(|agent| !agent.is_empty())
            .unwrap_or_else(|| header_user_agent(parts));
        return (source_ip, user_agent);
    }
    (String::new(), header_user_agent(parts))
}

fn header_user_agent(parts: &axum::http::request::Parts) -> String {
    parts
        .headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}
async fn log_response_body(
    response: Response,
    uri: &Uri,
) -> Result<lambda_http::Response<axum::body::Body>, BasicResponse> {
    let (parts, body) = response.into_parts();

    // this won't work if the body is an long running stream
    let bytes = body
        .collect()
        .await
        .map_err(|_| internal_error("Middleware could not parse response body"))?
        .to_bytes();

    // We just want to log the body for debugging purposes
    posthog_capture_request(parts.status, uri, &bytes).await;

    Ok(Response::from_parts(parts, Body::from(bytes)))
}

async fn posthog_capture_request(status: StatusCode, uri: &Uri, body: &Bytes) {
    if !PostHogEvent::should_report_http_exception(status) {
        return;
    }
    if let Ok(api_key) = std::env::var("POSTHOG_API_KEY") {
        let body_str = String::from_utf8_lossy(body);
        let event = PostHogEvent::new_http_exception(api_key, body_str, status, uri);
        let posthog_client = client().await;
        if let Err(err) = posthog_client.capture(event).await {
            tracing::error!("Error sending event to PostHog: {}", err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, Uri};
    use http_body_util::BodyExt; // For converting the returned body back to bytes to verify integrity
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn test_log_request_body_standard_route() {
        let uri: Uri = "/api/v1/users".parse().unwrap();
        let json_data = r#"{"username": "john_doe", "password": "secret_password"}"#;

        let request = Request::builder()
            .uri(&uri)
            .body(Body::from(json_data))
            .unwrap();

        let result = log_request_body(request, &uri).await;
        assert!(result.is_ok());

        // Only the shape is logged, never the values.
        assert!(logs_contain(r#""username": "String""#));
        assert!(!logs_contain("john_doe"));
        assert!(!logs_contain("secret_password"));

        // Ensure the request body was reconstructed perfectly
        let mut returned_req = result.unwrap();
        let body_bytes = returned_req.body_mut().collect().await.unwrap().to_bytes();
        assert_eq!(body_bytes, json_data.as_bytes());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_request_body_sms_route_success() {
        let uri: Uri = "/cloudtalk/sms/webhook".parse().unwrap();
        let json_data = r#"{
            "from": "+1234567890",
            "text": "Your OTP code is 4432",
            "segments": 1,
            "is_delivered": true
        }"#;

        let request = Request::builder()
            .uri(&uri)
            .body(Body::from(json_data))
            .unwrap();

        let result = log_request_body(request, &uri).await;
        assert!(result.is_ok());

        // Verify the original sensitive values are NOT in the logs
        assert!(!logs_contain("+1234567890"));
        assert!(!logs_contain("Your OTP code is 4432"));

        // Verify that the types schema was successfully logged instead
        assert!(logs_contain("body_schema="));
        assert!(logs_contain(r#""from": "String""#));
        assert!(logs_contain(r#""text": "String""#));
        assert!(logs_contain(r#""segments": "Number""#));
        assert!(logs_contain(r#""is_delivered": "Bool""#));

        // Ensure payload preservation
        let mut returned_req = result.unwrap();
        let body_bytes = returned_req.body_mut().collect().await.unwrap().to_bytes();
        assert_eq!(body_bytes, json_data.as_bytes());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_request_body_sms_route_fallback() {
        let uri: Uri = "/cloudtalk/sms/webhook".parse().unwrap();
        let plain_text_data = "plain text body that is not JSON";

        let request = Request::builder()
            .uri(&uri)
            .body(Body::from(plain_text_data))
            .unwrap();

        let result = log_request_body(request, &uri).await;
        assert!(result.is_ok());

        // Verify that it fallback logs the body size instead of trying to read keys
        assert!(logs_contain("body_size="));
        assert!(logs_contain("note=\"Body is not a valid JSON object\""));

        // Verify original data is preserved
        let mut returned_req = result.unwrap();
        let body_bytes = returned_req.body_mut().collect().await.unwrap().to_bytes();
        assert_eq!(body_bytes, plain_text_data.as_bytes());
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_request_body_sms_route_logs_caller() {
        let uri: Uri = "/cloudtalk/sms".parse().unwrap();
        let context: lambda_http::aws_lambda_events::event::apigw::ApiGatewayV2httpRequestContext =
            serde_json::from_str(
                r#"{"http":{"method":"POST","sourceIp":"203.0.113.10","userAgent":"CloudTalk-Webhook"}}"#,
            )
            .expect("request context");
        let request = Request::builder()
            .uri(&uri)
            .extension(RequestContext::ApiGatewayV2(context))
            .body(Body::from("{}"))
            .unwrap();

        let result = log_request_body(request, &uri).await;
        assert!(result.is_ok());
        assert!(logs_contain("source_ip=203.0.113.10"));
        assert!(logs_contain("user_agent=CloudTalk-Webhook"));
        assert!(logs_contain("body_schema={}"));
    }

    #[tokio::test]
    #[traced_test]
    async fn test_log_request_body_email_route_keeps_list_sizes_not_addresses() {
        let uri: Uri = "/email/send".parse().unwrap();
        let json_data =
            r#"{"to": [], "bcc": ["pat@example.com", "sam@example.com"], "body": "Hi Pat"}"#;
        let request = Request::builder()
            .uri(&uri)
            .body(Body::from(json_data))
            .unwrap();

        assert!(log_request_body(request, &uri).await.is_ok());
        assert!(logs_contain(r#""to": "Array(0)""#));
        assert!(logs_contain(r#""bcc": "Array(2)""#));
        assert!(!logs_contain("pat@example.com"));
        assert!(!logs_contain("Hi Pat"));
    }
}
