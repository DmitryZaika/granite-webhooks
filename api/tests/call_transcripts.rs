mod common;

use api::app;
use api::state::AppState;
use api::transcribe::Transcriber;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// Company 101 (Olga, 106) has CloudTalk keys. Call 55 has a recording, call
// 56 an empty one.

async fn server(pool: MySqlPool, text: &'static str) -> TestServer {
    let polls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route(
            "/calls/recording/{file}",
            get(|Path(file): Path<String>| async move {
                let bytes: &[u8] = if file == "55.json" {
                    b"RIFF\0\0\0\0WAVEfmt "
                } else {
                    b""
                };
                ([("content-type", "audio/wav")], bytes)
            }),
        )
        .route(
            "/v2/upload",
            post(|body: Bytes| async move {
                assert!(body.starts_with(b"RIFF"));
                Json(json!({ "upload_url": "https://cdn.test/u1" }))
            }),
        )
        .route(
            "/v2/transcript",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["audio_url"], "https://cdn.test/u1");
                Json(json!({ "id": "t1" }))
            }),
        )
        .route(
            "/v2/transcript/{id}",
            get(move || {
                let polls = Arc::clone(&polls);
                async move {
                    if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Json(json!({ "status": "processing" }));
                    }
                    Json(json!({ "status": "completed", "text": text, "audio_duration": 90.0 }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&base)
        .with_transcriber(Transcriber::new(Some("aai"), &base));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn a_cloudtalk_recording_is_transcribed_and_billed(pool: MySqlPool) {
    let server = server(pool.clone(), " Hi, it's about the quartz. ").await;
    let body: Value = server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "55" }))
        .await
        .json();
    assert_eq!(body, json!({ "text": "Hi, it's about the quartz." }));
    let logged: (String, String, f64, bool) = sqlx::query_as(
        "SELECT feature, api_kind, CAST(audio_seconds AS DOUBLE), success <> 0 FROM ai_usage_log ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        logged,
        (
            "call_transcription".into(),
            "transcription".into(),
            90.0,
            true
        )
    );
    let empty = server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "56" }))
        .await;
    empty.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(empty.json::<Value>()["error"], "Recording not available");
    server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "../x" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn silence_is_unprocessable(pool: MySqlPool) {
    let server = server(pool, "  ").await;
    let res = server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "55" }))
        .await;
    res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        res.json::<Value>()["error"],
        "No speech detected in recording"
    );
}
