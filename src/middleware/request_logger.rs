use axum::{
    body::{Body, Bytes},
    extract::Request,
    middleware::Next,
    response::IntoResponse,
};
use http_body_util::BodyExt;
use lambda_http::tracing;

use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;

// middleware that shows how to consume the request body upfront
pub async fn print_request_body(
    request: Request,
    next: Next,
) -> Result<impl IntoResponse, BasicResponse> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();
    let request = buffer_request_body(request).await?;
    tracing::info!(
        "Incoming request: {} {} (routing path: {})",
        method,
        uri,
        path
    );

    Ok(next.run(request).await)
}

// the trick is to take the request apart, buffer the body, do what you need to do, then put
// the request back together
async fn buffer_request_body(request: Request) -> Result<Request, BasicResponse> {
    let (parts, body) = request.into_parts();

    // this won't work if the body is an long running stream
    let bytes = body
        .collect()
        .await
        .map_err(|_| internal_error("Middleware could not parse request body"))?
        .to_bytes();

    do_thing_with_request_body(&bytes);

    Ok(Request::from_parts(parts, Body::from(bytes)))
}

fn do_thing_with_request_body(bytes: &Bytes) {
    tracing::info!(body = ?bytes);
}
