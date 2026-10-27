use crate::common;

use api::app;
use api::state::AppState;
use api::transcribe::Transcriber;
use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// Company 101 (Olga, 106) has CloudTalk keys. Call 55 has a recording, call
// 56 an empty one.

async fn server(pool: MySqlPool, text: &'static str) -> TestServer {
    server_with(pool, text, Router::new()).await.0
}

/// The provider stand-in with `extra` routes, and its base URL.
async fn server_with(pool: MySqlPool, text: &'static str, extra: Router) -> (TestServer, String) {
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
        )
        .merge(extra);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&base)
        .with_transcriber(Transcriber::new(Some("aai"), &base));
    (TestServer::new(app(state)), base)
}

/// Serves `router` on a port of its own (another origin); its base URL.
async fn elsewhere(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// A route that records the `Authorization` header of each request and
/// answers 401, as a host fishing for the key would.
fn recording_auth(seen: &Arc<Mutex<Vec<Option<String>>>>) -> Router {
    let seen = Arc::clone(seen);
    Router::new().route(
        "/r.wav",
        get(move |headers: HeaderMap| {
            seen.lock().unwrap().push(
                headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_string()),
            );
            async { StatusCode::UNAUTHORIZED }
        }),
    )
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

/// A recording link from the request body is fetched only from `CloudTalk`:
/// any other host would get the company's API key when it answers 401
/// (and could be an internal address). It is not even asked once.
#[test_db::test(seed)]
async fn recording_links_off_cloudtalk_are_never_fetched(pool: MySqlPool) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let attacker = elsewhere(recording_auth(&seen)).await;
    let server = server(pool, "unused").await;
    for link in [
        format!("{attacker}/r.wav"),
        format!(" {attacker}/r.wav "),
        format!("{}/r.wav", attacker.replace("127.0.0.1", "localhost")),
    ] {
        server
            .post("/v1/call-transcripts")
            .with_session(sessions::OTHER_COMPANY_REP)
            .json(&json!({ "call_id": "56", "recording_link": link }))
            .await
            .assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    }
    assert!(seen.lock().unwrap().is_empty());
}

/// Links on `CloudTalk`'s own origin still work as before: tried without
/// credentials, then again with the company's key.
#[test_db::test(seed)]
async fn cloudtalk_links_are_retried_with_the_companys_key(pool: MySqlPool) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let links = Router::new().route(
        "/links/r.wav",
        get(move |headers: HeaderMap| {
            let auth = headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_string());
            log.lock().unwrap().push(auth.clone());
            async move {
                match auth {
                    Some(_) => (
                        StatusCode::OK,
                        [("content-type", "audio/wav")],
                        &b"RIFF\0\0\0\0WAVEfmt "[..],
                    ),
                    None => (
                        StatusCode::UNAUTHORIZED,
                        [("content-type", "text/plain")],
                        &b""[..],
                    ),
                }
            }
        }),
    );
    let (server, base) = server_with(pool, "Hello from the link.", links).await;
    let body: Value = server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "56", "recording_link": format!("{base}/links/r.wav") }))
        .await
        .json();
    assert_eq!(body, json!({ "text": "Hello from the link." }));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], None);
    // base64("seed-key:seed-secret"), company 101's seeded key.
    assert_eq!(
        seen[1].as_deref(),
        Some("Basic c2VlZC1rZXk6c2VlZC1zZWNyZXQ=")
    );
}

/// A `CloudTalk` link that redirects elsewhere never carries the key there.
#[test_db::test(seed)]
async fn a_redirect_off_cloudtalk_never_carries_the_key(pool: MySqlPool) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let attacker = elsewhere(recording_auth(&seen)).await;
    let target = format!("{attacker}/r.wav");
    let away = Router::new().route(
        "/links/away",
        get(move || {
            let target = target.clone();
            async move { (StatusCode::FOUND, [("location", target)]) }
        }),
    );
    let (server, base) = server_with(pool, "unused", away).await;
    server
        .post("/v1/call-transcripts")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({ "call_id": "56", "recording_link": format!("{base}/links/away") }))
        .await
        .assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    let seen = seen.lock().unwrap().clone();
    assert!(seen.iter().all(Option::is_none), "{seen:?}");
}

/// Links that are not web addresses are not fetched at all.
#[test_db::test(seed)]
async fn recording_links_must_be_web_addresses(pool: MySqlPool) {
    let server = server(pool, "unused").await;
    for link in [
        "file:///etc/passwd",
        "ftp://my.cloudtalk.io/r.wav",
        "my.cloudtalk.io/r.wav",
        "",
    ] {
        server
            .post("/v1/call-transcripts")
            .with_session(sessions::OTHER_COMPANY_REP)
            .json(&json!({ "call_id": "56", "recording_link": link }))
            .await
            .assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    }
}
