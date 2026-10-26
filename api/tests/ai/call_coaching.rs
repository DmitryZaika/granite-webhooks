use crate::common;

use api::ai::Ai;
use api::app;
use api::state::AppState;
use api::transcribe::Transcriber;
use axum::extract::Path;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Olga (106, company 101 with CloudTalk keys) is agent 5106 on 317-555-0999.
// Call 71 is a two-minute recorded call with Alice's number; 72 is internal
// and 73 too short to coach.

async fn server(pool: &MySqlPool) -> TestServer {
    sqlx::raw_sql(
        "UPDATE users SET cloudtalk_agent_id = '5106', cloudtalk_phone_number = '3175550999' WHERE id = 106;",
    )
    .execute(pool)
    .await
    .unwrap();
    let router = Router::new()
        .route(
            "/calls/index.json",
            get(|| async {
                Json(json!({ "responseData": { "data": [
                    { "Cdr": { "id": "71", "type": "outgoing", "talking_time": "120", "recorded": true,
                               "started_at": "2026-10-01 15:00:00", "public_external": "+13175550100" },
                      "Contact": { "name": "Alice L." } },
                    { "Cdr": { "id": "72", "type": "internal", "talking_time": "300", "started_at": "2026-10-02 15:00:00" } },
                    { "Cdr": { "id": "73", "type": "incoming", "talking_time": "30", "started_at": "2026-10-03 15:00:00" } },
                ] } }))
            }),
        )
        .route(
            "/calls/recording/{file}",
            get(|Path(_file): Path<String>| async { ([("content-type", "audio/wav")], b"RIFF\0\0\0\0WAVEfmt ".to_vec()) }),
        )
        .route("/v2/upload", post(|| async { Json(json!({ "upload_url": "https://cdn.test/u" })) }))
        .route("/v2/transcript", post(|| async { Json(json!({ "id": "t" })) }))
        .route(
            "/v2/transcript/{id}",
            get(|| async { Json(json!({ "status": "completed", "text": "Hi Alice, what's not working in your kitchen?", "audio_duration": 120.0 })) }),
        )
        .route(
            "/chat",
            post(|Json(body): Json<Value>| async move {
                let prompt = body["messages"][1]["content"].as_str().unwrap().to_string();
                assert!(prompt.contains("CRM customer name: Alice Lead"), "{prompt}");
                assert!(prompt.contains("Transcript:\nHi Alice"));
                Json(json!({
                    "choices": [{ "message": { "content": "{\"relevant\":true,\"skip_reason\":null,\"score\":81,\"advice\":\"Book the visit.\",\"summary\":\"Quartz kitchen\"}" } }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 5 },
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&base)
        .with_transcriber(Transcriber::new(Some("aai"), &base))
        .with_ai(Ai::new(Some("key"), &format!("{base}/chat")));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn the_coach_scores_a_call_once(pool: MySqlPool) {
    sqlx::query("UPDATE customers SET company_id = 101 WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool).await;
    let listed: Value = server
        .get("/v1/me/call-coaching")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(listed["eligible"], true);
    let calls = listed["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["status"], "idle");
    assert_eq!(calls[0]["phone_display"], "317-555-0100");
    let scored: Value = server
        .post("/v1/me/call-coaching")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "71" }))
        .await
        .json();
    let call = &scored["calls"][0];
    assert_eq!(call["status"], "ready");
    assert_eq!(call["score"], 81);
    assert_eq!(call["advice"], "Book the visit.");
    assert_eq!(call["customer_name"], "Alice Lead");
    assert_eq!(scored["analyzing"], false);
    let again: Value = server
        .get("/v1/me/call-coaching")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(again["calls"][0]["score"], 81);
    let ineligible: Value = server
        .get("/v1/me/call-coaching")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        ineligible,
        json!({ "eligible": false, "analyzing": false, "calls": [] })
    );
}
