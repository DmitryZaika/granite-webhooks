use axum::body::Bytes;
use axum::http::{StatusCode, Uri};
use axum::{
    body::Body,
    extract::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use lambda_http::tracing;

use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;
use crate::posthog::{PostHogEvent, client};
use serde_json::Value;
use std::collections::HashMap;

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

    // Redact body for SMS routes — they contain customer phone numbers and message text (PII)
    if uri.path().starts_with("/cloudtalk/sms/") {
        // Attempt to parse bytes as JSON to extract metadata
        if let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bytes) {
            // Map each key to its corresponding data type string
            let body_schema: HashMap<&str, &str> = map
                .iter()
                .map(|(key, value)| {
                    let type_name = match value {
                        Value::Null => "Null",
                        Value::Bool(_) => "Bool",
                        Value::Number(_) => "Number",
                        Value::String(_) => "String",
                        Value::Array(_) => "Array",
                        Value::Object(_) => "Object",
                    };
                    (key.as_str(), type_name)
                })
                .collect();

            tracing::info!(body_schema = ?body_schema, path = %uri.path());
        } else {
            // Fallback if the body isn't a valid JSON object (e.g., empty or plain text)
            tracing::info!(
                body_size = bytes.len(),
                path = %uri.path(),
                note = "Body is not a valid JSON object"
            );
        }
    } else {
        tracing::info!(body = ?bytes);
    }

    Ok(Request::from_parts(parts, Body::from(bytes)))
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

        // Verify that the body was logged exactly as-is (raw bytes/debug format)
        assert!(logs_contain("body="));

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
}
