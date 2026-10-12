mod common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

/// A server whose `OpenAI` stub always answers `reply`, and the requests it got.
async fn server(pool: &MySqlPool, reply: &'static str) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push(body);
                Json(json!({
                    "choices": [{ "message": { "content": reply } }],
                    "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_ai(Ai::new(Some("key"), &format!("http://{address}/chat")));
    (TestServer::new(app(state)), seen)
}

#[test_db::test(seed)]
async fn an_admin_drafts_an_email_template(pool: MySqlPool) {
    let (server, seen) = server(
        &pool,
        "```json\n{\"template_name\":\"Follow-up\",\"template_subject\":\"Hi {{customer.first_name}}\",\"template_body\":\"<p>Hello</p>\"}\n```",
    )
    .await;
    let body: Value = server
        .post("/v1/ai/email-template")
        .with_session(sessions::ADMIN)
        .json(&json!({ "prompt": "follow up after an estimate" }))
        .await
        .json();
    assert_eq!(
        body,
        json!({ "template_name": "Follow-up", "template_subject": "Hi {{customer.first_name}}", "template_body": "<p>Hello</p>" })
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(
        sent[0]["messages"][1]["content"],
        "Generate an email template based on this description: \"follow up after an estimate\""
    );
    server
        .post("/v1/ai/email-template")
        .with_session(sessions::REP)
        .json(&json!({ "prompt": "x" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_reply_without_a_template_is_a_bad_gateway(pool: MySqlPool) {
    let (server, _) = server(&pool, "Sorry, I can't.").await;
    server
        .post("/v1/ai/email-template")
        .with_session(sessions::ADMIN)
        .json(&json!({ "prompt": "follow up" }))
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
}
