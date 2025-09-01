use axum::body::Bytes;
use axum::http::StatusCode;
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
use crate::posthog::{Event, client};

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
        println!("Error response received: SENDING TO POSTHOG");
        return log_response_body(response).await;
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
) -> Result<lambda_http::Response<axum::body::Body>, BasicResponse> {
    let (parts, body) = response.into_parts();

    // this won't work if the body is an long running stream
    let bytes = body
        .collect()
        .await
        .map_err(|_| internal_error("Middleware could not parse response body"))?
        .to_bytes();

    // We just want to log the body for debugging purposes
    posthog_capture_request(parts.status, &bytes).await;

    Ok(Response::from_parts(parts, Body::from(bytes)))
}

async fn posthog_capture_request(status: StatusCode, body: &Bytes) {
    let api_key = std::env::var("POSTHOG_API_KEY").unwrap();
    let mut event = Event::new_anon("$exception");
    event.insert_prop("$exception_type", "HTTPError").unwrap();
    event.insert_prop("status", status.as_u16()).unwrap();
    event.insert_prop("path", "/telegram/webhook").unwrap();
    let body_str = String::from_utf8_lossy(body);
    let exp_message = format!("status={} body={}", status.as_u16(), body_str);
    event
        .insert_prop("$exception_message", exp_message)
        .unwrap();
    let posthog_client = client(api_key).await;
    let ph_client = posthog_client.capture(event).await.unwrap();
    println!(
        "{}, {}",
        ph_client.status().as_u16(),
        ph_client.text().await.unwrap()
    );
    println!("Error response received: SENT TO POSTHOG");
}
