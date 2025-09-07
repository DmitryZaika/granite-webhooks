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

// middleware that shows how to consume the request body upfront
pub async fn print_request_body(
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, BasicResponse> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();
    let request = log_request_body(request).await?;
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

async fn log_request_body(request: Request) -> Result<Request, BasicResponse> {
    let (parts, body) = request.into_parts();

    // this won't work if the body is an long running stream
    let bytes = body
        .collect()
        .await
        .map_err(|_| internal_error("Middleware could not parse request body"))?
        .to_bytes();

    // We just want to log the body for debugging purposes
    tracing::info!(body = ?bytes);

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
        posthog_client.capture(event).await.unwrap();
        println!("Error response received: SENT TO POSTHOG");
    }
}
