use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use lambda_http::tracing;

pub async fn logging_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();

    // Log the incoming request with both full URI and routing path
    tracing::info!(
        "Incoming request: {} {} (routing path: {})",
        method,
        uri,
        path
    );

    let response = next.run(request).await;

    // Optionally log the response status
    tracing::info!("Response status: {}", response.status());

    response
}
