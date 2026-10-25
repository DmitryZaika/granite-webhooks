//! Copying recordings from Telnyx to the private bucket
//! (`phone/recordings.rs`, `routes/telnyx_webhooks/recording.rs`): the copy
//! and its key, the size check that keeps Telnyx's copy, the state the
//! phone job retries from, and Call Control's `call.recording.saved`.
//!
//! Fixture calls use ids 980900+.

use crate::phone_common;

use api::phone::PhoneConfig;
use api::phone::recordings::{self, CopyOutcome, RecordingKind};
use api::storage::Storage;
use axum::http::StatusCode;
use axum_test::TestServer;
use phone_common::{
    PUBLIC_API, Stubs, add_call, enable_calling, hook_path, phone_server_with, phone_state,
    post_texml, sql,
};
use serde_json::json;
use sqlx::MySqlPool;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const MP3: &[u8] = b"ID3\x04\x00fake mp3 frames for the test";

/// An answered incoming call that started 2026-10-10 10:00 UTC.
async fn answered_call(pool: &MySqlPool, id: i64, ai_notes: bool) {
    add_call(
        pool,
        id,
        100,
        Some(100),
        "inbound",
        "answered",
        "+13175550199",
        "2026-10-10 10:00:00",
        &format!(
            "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:01:05', duration_secs = 60,
             ai_notes = {}, telnyx_call_sid = 'v3:rec-{id}'",
            u8::from(ai_notes)
        ),
    )
    .await;
}

/// Telnyx's recording callback for call `id`.
async fn ready(server: &TestServer, id: i64, kind: &str, recording: &str, status: &str) {
    let sid = format!("v3:rec-{id}");
    post_texml(
        server,
        100,
        &format!("recording?call={id}&kind={kind}"),
        &[
            ("CallSid", sid.as_str()),
            ("RecordingSid", recording),
            ("RecordingStatus", status),
            ("RecordingDuration", "61"),
            ("RecordingChannels", "2"),
            ("RecordingUrl", "https://elsewhere.test/never-fetched.mp3"),
        ],
    )
    .await
    .assert_status_ok();
}

/// A call's recording state: (status, key, Telnyx id, seconds, attempts,
/// transcript status).
async fn recording_state(
    pool: &MySqlPool,
    id: i64,
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<i32>,
    i32,
    String,
) {
    sqlx::query_as(
        "SELECT CAST(recording_status AS CHAR), recording_key, recording_telnyx_id, recording_secs,
                CAST(recording_attempts AS SIGNED), CAST(transcript_status AS CHAR)
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Telnyx's copy of the call's recording was deleted.
async fn telnyx_deleted(pool: &MySqlPool, id: i64) -> bool {
    sqlx::query_scalar("SELECT telnyx_deleted_at IS NOT NULL FROM phone_calls WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A server whose bucket the test can read.
async fn server(pool: &MySqlPool) -> (TestServer, Stubs, Storage) {
    let storage = Storage::memory();
    let bucket = storage.clone();
    let (server, stubs) =
        phone_server_with(pool.clone(), move |state| state.with_storage(bucket)).await;
    (server, stubs, storage)
}

#[test_db::test(seed)]
async fn recordings_are_copied_to_the_private_bucket(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    answered_call(&pool, 980_901, true).await;
    let (server, stubs, storage) = server(&pool).await;
    let link = stubs.add_file("rec-1.mp3", MP3);
    stubs.add_recording("rec-1", &link, 61_500, "v3:rec-980901");
    ready(&server, 980_901, "call", "rec-1", "completed").await;
    let stored = storage
        .get("phone/100/2026/10/980901-call.mp3")
        .await
        .unwrap()
        .expect("copied");
    assert_eq!(
        (stored.bytes.as_slice(), stored.content_type.as_str()),
        (MP3, "audio/mpeg")
    );
    // Looked up with our key, then Telnyx's copy deleted.
    assert_eq!(
        stubs.telnyx_paths(),
        ["GET /recordings/rec-1", "DELETE /recordings/rec-1"]
    );
    // The copy counted as one attempt.
    assert_eq!(
        recording_state(&pool, 980_901).await,
        (
            "copied".into(),
            Some("phone/100/2026/10/980901-call.mp3".into()),
            Some("rec-1".into()),
            Some(62),
            1,
            "pending".into()
        )
    );
    assert!(telnyx_deleted(&pool, 980_901).await);
    // Sent again, or still in progress: nothing more.
    ready(&server, 980_901, "call", "rec-1", "completed").await;
    ready(&server, 980_901, "call", "rec-1", "in-progress").await;
    assert_eq!(stubs.telnyx().len(), 2);
    // A voicemail is queued for its AI note even when the user's AI notes
    // are off.
    answered_call(&pool, 980_902, false).await;
    let link = stubs.add_file("rec-2.mp3", MP3);
    stubs.add_recording("rec-2", &link, 9_000, "v3:rec-980902");
    ready(&server, 980_902, "voicemail", "rec-2", "completed").await;
    let (status, key, _, _, _, transcript) = recording_state(&pool, 980_902).await;
    assert_eq!(
        (status.as_str(), key.as_deref(), transcript.as_str()),
        (
            "copied",
            Some("phone/100/2026/10/980902-voicemail.mp3"),
            "pending"
        )
    );
    // An incoming call's recording with AI notes off is not.
    answered_call(&pool, 980_903, false).await;
    let link = stubs.add_file("rec-3.mp3", MP3);
    stubs.add_recording("rec-3", &link, 9_000, "v3:rec-980903");
    ready(&server, 980_903, "call", "rec-3", "completed").await;
    assert_eq!(recording_state(&pool, 980_903).await.5, "none");
    // An answered outgoing call is, AI notes or not: its transcript scores it.
    add_call(
        &pool,
        980_904,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550199",
        "2026-10-10 10:00:00",
        "answered_at = '2026-10-10 10:00:05', ended_at = '2026-10-10 10:01:05', duration_secs = 60,
         ai_notes = 0, telnyx_call_sid = 'v3:rec-980904'",
    )
    .await;
    let link = stubs.add_file("rec-4.mp3", MP3);
    stubs.add_recording("rec-4", &link, 9_000, "v3:rec-980904");
    ready(&server, 980_904, "call", "rec-4", "completed").await;
    assert_eq!(recording_state(&pool, 980_904).await.5, "pending");
    post_texml(
        &server,
        100,
        "recording?call=980903&kind=video",
        &[("RecordingSid", "rec-3"), ("RecordingStatus", "completed")],
    )
    .await
    .assert_status(StatusCode::BAD_REQUEST);
}

/// A server on 127.0.0.1 that promises `promised` bytes and sends 10, the
/// link to it.
fn short_download(promised: usize) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut socket in listener.incoming().flatten() {
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {promised}\r\nConnection: close\r\n\r\n"
            );
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(&MP3[..10]);
            let _ = socket.flush();
        }
    });
    format!("http://{address}/short.mp3")
}

#[test_db::test(seed)]
async fn a_copy_that_does_not_check_out_keeps_telnyxs_copy(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    answered_call(&pool, 980_911, true).await;
    answered_call(&pool, 980_912, true).await;
    answered_call(&pool, 980_913, true).await;
    let (state, stubs) = phone_state(pool.clone()).await;
    let server = TestServer::new(api::app(state.clone()));
    // The download comes up short of its Content-Length.
    stubs.add_recording("rec-short", &short_download(1000), 5_000, "v3:rec-980911");
    ready(&server, 980_911, "call", "rec-short", "completed").await;
    assert_eq!(stubs.telnyx_paths(), ["GET /recordings/rec-short"]);
    assert_eq!(
        recording_state(&pool, 980_911).await,
        (
            "pending".into(),
            None,
            Some("rec-short".into()),
            Some(61),
            1,
            "none".into()
        )
    );
    assert!(state.storage.memory_keys().is_empty());
    // Telnyx is down.
    stubs.clear_telnyx();
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    ready(&server, 980_912, "call", "rec-down", "completed").await;
    stubs.telnyx_down.store(false, Ordering::SeqCst);
    assert_eq!(stubs.telnyx_paths(), ["GET /recordings/rec-down"]);
    assert_eq!(recording_state(&pool, 980_912).await.4, 1);
    // Something else is stored under the call's key: ours is not written
    // over it, and Telnyx's copy stays.
    stubs.clear_telnyx();
    state
        .storage
        .put_private(
            "phone-recordings",
            "phone/100/2026/10/980913-call.mp3",
            b"other bytes".to_vec(),
            "audio/mpeg",
        )
        .await
        .unwrap();
    let link = stubs.add_file("rec-13.mp3", MP3);
    stubs.add_recording("rec-13", &link, 5_000, "v3:rec-980913");
    ready(&server, 980_913, "call", "rec-13", "completed").await;
    assert_eq!(stubs.telnyx_paths(), ["GET /recordings/rec-13"]);
    let (status, key, _, _, attempts, _) = recording_state(&pool, 980_913).await;
    assert_eq!((status.as_str(), key, attempts), ("pending", None, 1));
    // The phone job's retry (the same copy, later) goes through once the
    // file is whole: the second attempt.
    stubs.clear_telnyx();
    let link = stubs.add_file("rec-short.mp3", MP3);
    stubs.add_recording("rec-short", &link, 5_000, "v3:rec-980911");
    let outcome = recordings::copy(&state, 100, 980_911, "rec-short", RecordingKind::Call, None)
        .await
        .unwrap();
    assert_eq!(outcome, CopyOutcome::Copied);
    assert_eq!(
        stubs.telnyx_paths(),
        ["GET /recordings/rec-short", "DELETE /recordings/rec-short"]
    );
    let (status, key, _, secs, attempts, transcript) = recording_state(&pool, 980_911).await;
    assert_eq!(
        (
            status.as_str(),
            key.as_deref(),
            secs,
            attempts,
            transcript.as_str()
        ),
        (
            "copied",
            Some("phone/100/2026/10/980911-call.mp3"),
            Some(5),
            2,
            "pending"
        )
    );
    assert_eq!(
        recordings::copy(&state, 100, 980_911, "rec-short", RecordingKind::Call, None)
            .await
            .unwrap(),
        CopyOutcome::AlreadyCopied
    );
    // Another company's call is not copied.
    assert_eq!(
        recordings::copy(&state, 101, 980_912, "rec-x", RecordingKind::Call, None)
            .await
            .unwrap(),
        CopyOutcome::Failed("no such call")
    );
}

#[test_db::test(seed)]
async fn only_the_calls_own_recording_is_copied(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    answered_call(&pool, 980_914, true).await;
    answered_call(&pool, 980_915, true).await;
    answered_call(&pool, 980_916, true).await;
    let (server, stubs, storage) = server(&pool).await;
    let link = stubs.add_file("other.mp3", MP3);
    // Telnyx says the recording is of another call, of another company's
    // connection, or of no call it names.
    stubs.add_recording("rec-other-call", &link, 5_000, "v3:someone-else");
    stubs.add_recording_of(
        "rec-other-connection",
        &link,
        5_000,
        json!({ "call_control_id": "v3:rec-980915", "connection_id": "conn-999" }),
    );
    stubs.add_recording_of("rec-no-call", &link, 5_000, json!({}));
    ready(&server, 980_914, "call", "rec-other-call", "completed").await;
    ready(
        &server,
        980_915,
        "call",
        "rec-other-connection",
        "completed",
    )
    .await;
    ready(&server, 980_916, "call", "rec-no-call", "completed").await;
    // Looked up only: nothing stored, nothing deleted, nothing left to retry.
    assert_eq!(
        stubs.telnyx_paths(),
        [
            "GET /recordings/rec-other-call",
            "GET /recordings/rec-other-connection",
            "GET /recordings/rec-no-call"
        ]
    );
    assert!(storage.memory_keys().is_empty());
    for id in [980_914, 980_915, 980_916] {
        assert_eq!(
            recording_state(&pool, id).await,
            ("none".into(), None, None, None, 0, "none".into()),
            "{id}"
        );
    }
    // The same call's leg, or its session, is the call's.
    stubs.clear_telnyx();
    sql(&pool, "INSERT INTO phone_call_legs (call_id, device_id, leg_call_sid, status) VALUES (980914, NULL, 'leg-914', 'completed')").await;
    stubs.add_recording_of(
        "rec-leg",
        &stubs.add_file("leg.mp3", MP3),
        5_000,
        json!({ "call_control_id": "leg-914", "connection_id": "app-100" }),
    );
    ready(&server, 980_914, "call", "rec-leg", "completed").await;
    assert_eq!(recording_state(&pool, 980_914).await.0, "copied");
}

#[test_db::test(seed)]
async fn a_failed_delete_of_telnyxs_copy_is_left_for_the_job(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    answered_call(&pool, 980_917, true).await;
    let (state, stubs) = phone_state(pool.clone()).await;
    let link = stubs.add_file("rec-17.mp3", MP3);
    stubs.add_recording("rec-17", &link, 5_000, "v3:rec-980917");
    // Telnyx fails only the delete.
    stubs.fail_deletes.store(true, Ordering::SeqCst);
    let outcome = recordings::copy(&state, 100, 980_917, "rec-17", RecordingKind::Call, None)
        .await
        .unwrap();
    assert_eq!(outcome, CopyOutcome::Copied);
    assert!(!telnyx_deleted(&pool, 980_917).await);
    let tries: (i64, bool) = sqlx::query_as(
        "SELECT CAST(telnyx_delete_attempts AS SIGNED), telnyx_delete_tried_at IS NOT NULL
           FROM phone_calls WHERE id = 980917",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tries, (1, true));
}

#[test_db::test(seed)]
async fn without_a_bucket_recordings_wait_for_the_job(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    answered_call(&pool, 980_921, true).await;
    let (server, stubs) = phone_server_with(pool.clone(), |state| {
        state.with_phone_config(PhoneConfig {
            public_api_url: Arc::from(PUBLIC_API),
            ..PhoneConfig::default()
        })
    })
    .await;
    ready(&server, 980_921, "call", "rec-21", "completed").await;
    assert!(stubs.telnyx().is_empty());
    assert_eq!(
        recording_state(&pool, 980_921).await,
        (
            "pending".into(),
            None,
            Some("rec-21".into()),
            Some(61),
            1,
            "none".into()
        )
    );
}

#[test_db::test(seed)]
async fn call_control_recordings_are_copied_too(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_call(
        &pool,
        980_931,
        100,
        Some(100),
        "outbound",
        "answered",
        "+13175550142",
        "2026-09-30 23:59:00",
        "answered_at = '2026-09-30 23:59:10', telnyx_call_control_id = 'v3:app-r',
         telnyx_call_session_id = 'session-r', approved = 1, ai_notes = 1",
    )
    .await;
    let (server, stubs, storage) = server(&pool).await;
    let link = stubs.add_file("rec-cc.mp3", MP3);
    // Telnyx names the customer's leg and the call's session.
    stubs.add_recording_of(
        "rec-cc",
        &link,
        70_000,
        json!({ "call_control_id": "v3:customer-leg", "call_session_id": "session-r",
                "connection_id": "conn-100" }),
    );
    // Telnyx's own link in the event is never fetched.
    let saved = json!({ "data": {
        "record_type": "event", "id": "evt-rec-1", "event_type": "call.recording.saved",
        "occurred_at": "2026-09-30T23:59:59Z",
        "payload": { "call_control_id": "v3:customer-leg", "call_session_id": "session-r",
                     "connection_id": "conn-100", "recording_id": "rec-cc", "channels": "dual",
                     "recording_urls": { "mp3": "https://elsewhere.test/never.mp3" } } } });
    server
        .post(&hook_path(100, "outbound"))
        .json(&saved)
        .await
        .assert_status_ok();
    assert!(
        storage
            .get("phone/100/2026/09/980931-call.mp3")
            .await
            .unwrap()
            .is_some()
    );
    let (status, _, _, secs, _, transcript) = recording_state(&pool, 980_931).await;
    assert_eq!(
        (status.as_str(), secs, transcript.as_str()),
        ("copied", Some(70), "pending")
    );
    assert_eq!(
        stubs.telnyx_paths(),
        ["GET /recordings/rec-cc", "DELETE /recordings/rec-cc"]
    );
}
