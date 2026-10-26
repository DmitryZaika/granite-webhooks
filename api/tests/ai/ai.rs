use crate::common;

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

async fn server(pool: &MySqlPool) -> (TestServer, Arc<Mutex<Vec<Value>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let router = Router::new().route(
        "/chat",
        post(move |Json(body): Json<Value>| {
            let calls = Arc::clone(&calls);
            async move {
                calls.lock().unwrap().push(body);
                Json(json!({
                    "choices": [{ "message": { "content": "  Hi Alice, thanks for your patience.  " } }],
                    "usage": { "prompt_tokens": 1000, "completion_tokens": 20 },
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
async fn a_draft_is_improved_and_billed(pool: MySqlPool) {
    let (server, seen) = server(&pool).await;
    let res = server
        .post("/v1/ai/improve")
        .with_session(sessions::REP)
        .json(&json!({ "body": "hi alise thx", "channel": "sms", "customer_name": "Alice Lead" }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "body": "Hi Alice, thanks for your patience." })
    );
    let sent = seen.lock().unwrap().clone();
    let messages = sent[0]["messages"].as_array().unwrap();
    assert_eq!(sent[0]["model"], "gpt-6-luna");
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("You edit SMS text messages.")
    );
    assert!(
        messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("\"Alice\"")
    );
    assert_eq!(
        messages[2],
        json!({ "role": "user", "content": "hi alise thx" })
    );
    let logged: (String, i32, i64, bool) = sqlx::query_as(
        "SELECT feature, user_id, CAST(input_tokens AS SIGNED), success <> 0 FROM ai_usage_log ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, ("email_improve".into(), 100, 1000, true));
    server
        .post("/v1/ai/improve")
        .with_session(sessions::REP)
        .json(&json!({ "body": "" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_voicemail_note_is_short(pool: MySqlPool) {
    let (server, seen) = server(&pool).await;
    let res = server
        .post("/v1/ai/call-note")
        .with_session(sessions::REP)
        .json(&json!({ "transcript": "Hi, this is Rita calling back about your quote.", "voicemail": true }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>()["content"],
        "Hi Alice, thanks for your patience."
    );
    let sent = seen.lock().unwrap().clone();
    assert_eq!(sent[0]["max_completion_tokens"], 180);
    assert!(
        sent[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("Summarize this outbound sales voicemail")
    );
    server
        .post("/v1/ai/call-note")
        .with_session(sessions::REP)
        .json(&json!({ "transcript": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
