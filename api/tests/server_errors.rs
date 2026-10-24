//! What the API tells PostHog about the 5xx it answers (`lib.rs`'s
//! `report_server_errors`): the route's template, never the path itself,
//! whose segments carry the company's Telnyx webhook token or a customer's
//! number. No database: one that never answers makes the routes fail.

use api::app;
use api::state::AppState;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::Value;
use sqlx::mysql::MySqlPoolOptions;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// PostHog's capture endpoint on 127.0.0.1: its root, and the events sent.
async fn posthog() -> (String, Arc<Mutex<Vec<Value>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&events);
    let router = Router::new().fallback(move |request: Request| {
        let seen = Arc::clone(&seen);
        async move {
            let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
                .await
                .unwrap();
            seen.lock()
                .unwrap()
                .push(serde_json::from_slice(&bytes).unwrap());
            StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, events)
}

#[tokio::test]
async fn server_errors_are_reported_by_route_never_by_path() {
    let (host, events) = posthog().await;
    // SAFETY: set before the server answers anything; this binary has no
    // other test.
    unsafe {
        std::env::set_var("POSTHOG_SERVER_ENABLED", "true");
        std::env::set_var("POSTHOG_KEY", "phc_test");
        std::env::set_var("POSTHOG_HOST", &host);
    }
    let pool = MySqlPoolOptions::new()
        .acquire_timeout(Duration::from_millis(300))
        .connect_lazy("mysql://root:granite@127.0.0.1:9/nowhere")
        .unwrap();
    let server = TestServer::new(app(AppState::new(
        pool,
        "test-secret",
        "http://localhost:5173",
    )));
    // A Telnyx callback (its path names the company's webhook token) and a
    // text thread of the app (its key ends in the customer's number).
    server
        .post("/v1/webhooks/telnyx/tok00000000000000000000secret77/outbound")
        .json(&serde_json::json!({}))
        .await
        .assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    server
        .post("/v1/phone/texts/threads/9702-3175550102/read")
        .add_header("x-granite-device", "0".repeat(43))
        .await
        .assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    let events = events.lock().unwrap().clone();
    let routes: Vec<&str> = events
        .iter()
        .map(|event| event["properties"]["api_path"].as_str().unwrap())
        .collect();
    assert_eq!(
        routes,
        [
            "/v1/webhooks/telnyx/{token}/outbound",
            "/v1/phone/texts/threads/{key}/read"
        ]
    );
    for event in &events {
        assert_eq!(event["event"], "$exception");
        assert_eq!(event["properties"]["api_status"], 500);
        let sent = event.to_string();
        assert!(!sent.contains("secret77"), "{sent}");
        assert!(!sent.contains("3175550102"), "{sent}");
    }
}
