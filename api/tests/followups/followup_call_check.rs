//! `POST /v1/followups/call-check`: after a rep's outbound call, whether the
//! customer talked back (and their running flows stop).

use crate::common;

use api::app;
use api::routes::followups::call_check::RecordingRetryDelay;
use api::state::AppState;
use api::transcribe::Transcriber;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Company 101 has CloudTalk keys; its customer 2000 is 463-555-0200. Call 55
// has a recording, call 56 an empty one. Two runs: one by phone, one by the
// customer.

const FIXTURE: &str = "
INSERT INTO sms_flows (id, company_id, user_id, name) VALUES (9700, 101, 106, 'Olga flow');
INSERT INTO sms_flow_steps (flow_id, position, channel, custom_text, delay_minutes) VALUES (9700, 1, 'sms', 'Hi', 1440);
INSERT INTO sms_flow_enrollments (id, flow_id, company_id, customer_phone_digits, customer_id, user_id, status,
                                  current_step_position, anchor_at, next_send_at) VALUES
  (9800, 9700, 101, 4635550200, NULL, 106, 'active', 1, '2026-10-01 12:00:00', '2026-10-20 12:00:00'),
  (9801, 9700, 101, NULL, 2000, 106, 'paused', 1, '2026-10-01 12:00:00', '2026-10-20 12:00:00');
";

async fn server(pool: &MySqlPool, utterances: Value) -> TestServer {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
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
            post(|| async { Json(json!({ "upload_url": "https://cdn.test/u1" })) }),
        )
        .route(
            "/v2/transcript",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["speaker_labels"], true);
                Json(json!({ "id": "t1" }))
            }),
        )
        .route(
            "/v2/transcript/{id}",
            get(move || {
                let utterances = utterances.clone();
                async move {
                    let text = utterances
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|u| u["text"].as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join(" ");
                    Json(json!({ "status": "completed", "text": text, "utterances": utterances, "audio_duration": 60.0 }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&base)
        .with_transcriber(Transcriber::new(Some("aai"), &base))
        .with_job_key("job-secret")
        // A missing recording is tried 4 times, 2.5 s apart in production.
        .with_extension(RecordingRetryDelay(std::time::Duration::from_millis(10)));
    TestServer::new(app(state))
}

fn conversation() -> Value {
    json!([
        { "speaker": "A", "text": "Hi, this is Olga from Other Granite about your countertops" },
        { "speaker": "B", "text": "Oh hi, yes I was going to call you back today actually" },
        { "speaker": "A", "text": "Great, did you have any questions about the quote I sent" },
        { "speaker": "B", "text": "Just the edge profile, can we do a bevel on the island" }
    ])
}

fn greeting() -> Value {
    json!([
        { "speaker": "A", "text": "The person you are calling is not available. Please leave a message after the tone." }
    ])
}

async fn check(server: &TestServer, talking_time: i64, call_id: i64) -> Value {
    let res = server
        .post("/v1/followups/call-check")
        .add_header("Authorization", "job-secret")
        .json(&json!({
            "company_id": 101,
            "phone_digits": 14_635_550_200_u64,
            "call_id": call_id,
            "talking_time": talking_time,
            "is_voicemail": false,
        }))
        .await;
    res.assert_status_ok();
    res.json()
}

async fn statuses(pool: &MySqlPool) -> Vec<String> {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id IN (9800, 9801) ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn points(pool: &MySqlPool) -> Option<(String, i32, String)> {
    sqlx::query_as(
        "SELECT CAST(outcome AS CHAR), talk_seconds, customer_digits FROM phone_call_points
          WHERE company_id = 101 AND provider = 'cloudtalk' AND provider_call_id = '55'",
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_short_call_is_not_checked(pool: MySqlPool) {
    let server = server(&pool, conversation()).await;
    let body = check(&server, 30, 55).await;
    assert_eq!(
        body,
        json!({ "skipped": true, "reason": "talking_time_too_short", "cancelled": 0,
                "customer_responded": false, "transcript_preview": null })
    );
    assert_eq!(statuses(&pool).await, ["active", "paused"]);
}

#[test_db::test(seed)]
async fn a_long_call_is_a_conversation_without_a_transcript(pool: MySqlPool) {
    let server = server(&pool, greeting()).await;
    let body = check(&server, 91, 55).await;
    assert_eq!(body["reason"], "long_call");
    assert_eq!(body["customer_responded"], true);
    // By phone and by the customer the phone belongs to.
    assert_eq!(body["cancelled"], 2);
    assert_eq!(
        statuses(&pool).await,
        ["stopped_by_reply", "stopped_by_reply"]
    );
    assert_eq!(points(&pool).await, None);
}

#[test_db::test(seed)]
async fn a_back_and_forth_stops_the_flows_and_scores_the_call(pool: MySqlPool) {
    let server = server(&pool, conversation()).await;
    let body = check(&server, 60, 55).await;
    assert_eq!(body["reason"], "customer_conversation");
    assert_eq!(body["skipped"], false);
    assert_eq!(body["cancelled"], 2);
    assert!(
        body["transcript_preview"]
            .as_str()
            .unwrap()
            .starts_with("Hi, this is Olga")
    );
    assert_eq!(
        statuses(&pool).await,
        ["stopped_by_reply", "stopped_by_reply"]
    );
    assert_eq!(
        points(&pool).await,
        Some(("conversation".into(), 60, "4635550200".into()))
    );
}

#[test_db::test(seed)]
async fn a_mailbox_greeting_leaves_the_flows_running(pool: MySqlPool) {
    let server = server(&pool, greeting()).await;
    let body = check(&server, 60, 55).await;
    assert_eq!(body["reason"], "voicemail");
    assert_eq!(body["customer_responded"], false);
    assert_eq!(statuses(&pool).await, ["active", "paused"]);
    assert_eq!(
        points(&pool).await,
        Some(("attempt".into(), 60, "4635550200".into()))
    );
}

#[test_db::test(seed)]
async fn a_missing_recording_is_retried_then_skipped(pool: MySqlPool) {
    let server = server(&pool, conversation()).await;
    let body = check(&server, 60, 56).await;
    assert_eq!(body["reason"], "transcription_unavailable");
    assert_eq!(statuses(&pool).await, ["active", "paused"]);
}

#[test_db::test(seed)]
async fn the_check_needs_the_key(pool: MySqlPool) {
    let server = server(&pool, conversation()).await;
    server
        .post("/v1/followups/call-check")
        .json(&json!({ "company_id": 101, "phone_digits": "4635550200", "call_id": 55, "talking_time": 60 }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
