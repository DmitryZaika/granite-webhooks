//! The Record button of a live call (`routes/phone/recording.rs`,
//! `PUT /v1/phone/calls/recording`): Call Control on the browser's (or the
//! app's) leg with the notice, the TeXML REST recording API on calls our
//! TeXML app answered, and when it is refused. Telnyx is a stand-in that
//! keeps every request, form bodies included.

mod common;
mod phone_common;

use api::app;
use api::telnyx::Telnyx;
use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum_test::TestServer;
use common::{WithSession, sessions};
use phone_common::{TELNYX_KEY, add_call, enable_calling, phone_state};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

/// What the stand-in got: (method, path, body as sent).
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, String)>>>);

impl Seen {
    fn all(&self) -> Vec<(String, String, String)> {
        self.0.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// The JSON body of request `i`.
    fn json(&self, i: usize) -> Value {
        serde_json::from_str(&self.all()[i].2).unwrap()
    }

    fn paths(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect()
    }
}

/// The API with Telnyx at a stand-in that lists one in-progress recording,
/// `rec-1`, for TeXML call `v3:in-1`.
async fn server(pool: MySqlPool) -> (TestServer, Seen) {
    let (state, _stubs) = phone_state(pool).await;
    let seen = Seen::default();
    let keep = seen.clone();
    let router = Router::new().fallback(move |request: Request| {
        let keep = keep.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
            let path = parts.uri.path().to_string();
            keep.0.lock().unwrap().push((
                parts.method.to_string(),
                path.clone(),
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
            let answer: Response = if path.ends_with("/Calls/v3:in-1/Recordings.json") {
                Json(json!({ "recordings": [
                    { "sid": "rec-0", "status": "completed" },
                    { "sid": "rec-1", "status": "in-progress" } ] }))
                .into_response()
            } else if path.ends_with("/Recordings.json") {
                Json(json!({ "recordings": [] })).into_response()
            } else {
                Json(json!({ "data": {} })).into_response()
            };
            answer
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = state.with_telnyx(Telnyx::new(Some(TELNYX_KEY), &base));
    (TestServer::new(app(state)), seen)
}

/// Now, as the fixtures write call start times.
fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

async fn record(server: &TestServer, recording: bool, ids: &[&str]) -> axum_test::TestResponse {
    server
        .put("/v1/phone/calls/recording")
        .with_session(sessions::REP)
        .json(&json!({ "recording": recording, "telnyxCallIds": ids }))
        .await
}

async fn live_state(pool: &MySqlPool, id: i64) -> (String, Option<String>) {
    sqlx::query_as(
        "SELECT CAST(live_recording AS CHAR), live_recording_sid FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn an_outgoing_browser_call_is_recorded_after_the_notice(pool: MySqlPool) {
    // The company records incoming calls only: outgoing ones start unrecorded.
    enable_calling(&pool, 100, "").await;
    add_call(
        &pool,
        981_001,
        100,
        Some(100),
        "outbound",
        "in_progress",
        "+13175550142",
        &now(),
        "telnyx_call_control_id = 'v3:web-1', telnyx_call_session_id = 'sess-1',
         our_number = '+13175550171', answered_at = UTC_TIMESTAMP()",
    )
    .await;
    let (server, seen) = server(pool.clone()).await;

    let started = record(&server, true, &["v3:web-1", "sess-1"]).await;
    started.assert_status_ok();
    assert_eq!(
        started.json::<Value>(),
        json!({ "callId": 981_001, "recording": true })
    );
    assert_eq!(
        seen.paths(),
        [
            "POST /calls/v3:web-1/actions/speak",
            "POST /calls/v3:web-1/actions/record_start"
        ]
    );
    let speak = seen.json(0);
    assert_eq!(
        (&speak["payload"], &speak["target_legs"], &speak["voice"]),
        (
            &json!("This call is being recorded."),
            &json!("both"),
            &json!("female")
        )
    );
    let start = seen.json(1);
    assert_eq!(
        (&start["format"], &start["channels"]),
        (&json!("mp3"), &json!("dual"))
    );
    assert_eq!(live_state(&pool, 981_001).await.0, "recording");

    // Already recording: nothing to do.
    seen.clear();
    record(&server, true, &[]).await.assert_status_ok();
    assert!(seen.all().is_empty());

    // Pause, then resume (the notice again).
    record(&server, false, &[]).await.assert_status_ok();
    assert_eq!(seen.paths(), ["POST /calls/v3:web-1/actions/record_pause"]);
    assert_eq!(live_state(&pool, 981_001).await.0, "paused");
    seen.clear();
    let resumed = record(&server, true, &[]).await;
    assert_eq!(resumed.json::<Value>()["recording"], true);
    assert_eq!(
        seen.paths(),
        [
            "POST /calls/v3:web-1/actions/speak",
            "POST /calls/v3:web-1/actions/record_resume"
        ]
    );

    // With the company's own notice recording, that is played instead.
    sqlx::query(
        "UPDATE telnyx_company_config SET recording_notice_url = 'https://cdn.test/notice.mp3'",
    )
    .execute(&pool)
    .await
    .unwrap();
    record(&server, false, &[]).await.assert_status_ok();
    seen.clear();
    record(&server, true, &[]).await.assert_status_ok();
    assert_eq!(
        seen.paths()[0],
        "POST /calls/v3:web-1/actions/playback_start"
    );
    assert_eq!(seen.json(0)["audio_url"], "https://cdn.test/notice.mp3");
}

#[test_db::test(seed)]
async fn an_incoming_call_is_paused_and_resumed_through_texml(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_call(
        &pool,
        981_002,
        100,
        Some(100),
        "inbound",
        "in_progress",
        "+13175550142",
        &now(),
        "telnyx_call_sid = 'v3:in-1', our_number = '+13175550171', answered_at = UTC_TIMESTAMP()",
    )
    .await;
    let (server, seen) = server(pool.clone()).await;

    // Recorded from the answer (the company records incoming calls).
    let paused = record(&server, false, &["v3:browser-leg"]).await;
    paused.assert_status_ok();
    assert_eq!(
        paused.json::<Value>(),
        json!({ "callId": 981_002, "recording": false })
    );
    assert_eq!(
        seen.paths(),
        [
            "GET /texml/Accounts/acct-100/Calls/v3:in-1/Recordings.json",
            "POST /texml/Accounts/acct-100/Calls/v3:in-1/Recordings/rec-1.json"
        ]
    );
    assert_eq!(seen.all()[1].2, "Status=paused");
    assert_eq!(
        live_state(&pool, 981_002).await,
        ("paused".into(), Some("v3:in-1|rec-1".into()))
    );
    seen.clear();
    record(&server, true, &[]).await.assert_status_ok();
    assert_eq!(
        seen.all(),
        [(
            "POST".to_string(),
            "/texml/Accounts/acct-100/Calls/v3:in-1/Recordings/rec-1.json".to_string(),
            "Status=in-progress".to_string()
        )]
    );
}

#[test_db::test(seed)]
async fn the_button_needs_recording_on_and_a_live_call(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'off'").await;
    let (server, seen) = server(pool.clone()).await;
    let off = record(&server, true, &[]).await;
    off.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(off.json::<Value>(), json!({ "error": "recording_off" }));
    sqlx::query("UPDATE telnyx_company_config SET recording_mode = 'inbound'")
        .execute(&pool)
        .await
        .unwrap();
    // On no call.
    record(&server, true, &[])
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Call via my cell when outgoing calls are not recorded: the customer
    // heard no notice, so it cannot start.
    add_call(
        &pool,
        981_003,
        100,
        Some(100),
        "outbound",
        "in_progress",
        "+13175550142",
        &now(),
        "via_cell = 1, telnyx_call_sid = 'v3:cell-1', answered_at = UTC_TIMESTAMP()",
    )
    .await;
    let refused = record(&server, true, &[]).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(
        refused.json::<Value>(),
        json!({ "error": "recording_unavailable" })
    );
    // Someone else's call, a viewer, no sign-in.
    server
        .put("/v1/phone/calls/recording")
        .with_session(sessions::MANAGER)
        .json(&json!({ "recording": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/phone/calls/recording")
        .with_session(sessions::VIEWER)
        .json(&json!({ "recording": true }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/phone/calls/recording")
        .json(&json!({ "recording": true }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert!(seen.all().is_empty());
}
