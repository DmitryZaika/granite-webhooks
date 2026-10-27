//! An incoming call after its phones rang (`routes/telnyx_webhooks/dial.rs`,
//! `status.rs`): `dial-result` and the no-answer chain (cells behind "press
//! 1", the AI Assistant, voicemail), declines, leg and call status
//! callbacks, the missed / voicemail / AI message pushes, the CRM
//! follow-through after a call, and the fallback TeXML.
//!
//! Fixture phones use n = 21..=40; numbers 9821+; customers 930001+.

use crate::phone_common;

use api::phone::PhoneConfig;
use axum::http::StatusCode;
use phone_common::{
    HEADER, PUBLIC_API, add_number, add_phone, assign, enable_calling, phone_server,
    phone_server_with, post_texml, set_settings, sql, texml_doc, token, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;

/// Rita's direct line and the sales line (Rita and Mark); a phone of
/// company 101 (Olga's).
async fn lines(pool: &MySqlPool) {
    add_number(pool, 9821, 100, "+13175550151", "direct", "Rita direct").await;
    add_number(pool, 9822, 100, "+13175550152", "shared", "Sales").await;
    assign(pool, 9821, 100, true, true).await;
    assign(pool, 9822, 100, true, true).await;
    assign(pool, 9822, 101, true, true).await;
}

/// A phone that cannot ring (no Telnyx credential) but gets our pushes.
async fn push_only_phone(pool: &MySqlPool, n: u32, user_id: i32) {
    let device = add_phone(pool, n, user_id, 100).await;
    sql(
        pool,
        &format!("UPDATE phone_devices SET sip_username = NULL, telnyx_credential_id = NULL WHERE id = {device}"),
    )
    .await;
}

async fn inbound(server: &axum_test::TestServer, sid: &str, from: &str, to: &str) -> String {
    let res = post_texml(
        server,
        100,
        "inbound",
        &[
            ("AccountSid", "acct-100"),
            ("CallSid", sid),
            ("From", from),
            ("To", to),
            ("CallStatus", "ringing"),
        ],
    )
    .await;
    res.assert_status_ok();
    res.text()
}

async fn call_id(pool: &MySqlPool, sid: &str) -> i64 {
    sqlx::query_scalar("SELECT CAST(id AS SIGNED) FROM phone_calls WHERE telnyx_call_sid = ?")
        .bind(sid)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A call's (status, user, answering phone, answered, ended, talk time).
async fn state(
    pool: &MySqlPool,
    id: i64,
) -> (String, Option<i32>, Option<i64>, bool, bool, Option<i32>) {
    sqlx::query_as(
        "SELECT CAST(status AS CHAR), user_id, CAST(answered_device_id AS SIGNED),
                answered_at IS NOT NULL, ended_at IS NOT NULL, duration_secs
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A leg status callback of phone `device` (or the customer's leg).
async fn leg(
    server: &axum_test::TestServer,
    id: i64,
    device: Option<i64>,
    sid: &str,
    status: &str,
) {
    let route = match device {
        Some(device) => format!("leg-status?call={id}&device={device}"),
        None => format!("leg-status?call={id}"),
    };
    post_texml(
        server,
        100,
        &route,
        &[
            ("AccountSid", "acct-100"),
            ("CallSid", sid),
            ("CallStatus", status),
        ],
    )
    .await
    .assert_status_ok();
}

async fn post(server: &axum_test::TestServer, route: &str, form: &[(&str, &str)]) -> String {
    let res = post_texml(server, 100, route, form).await;
    res.assert_status_ok();
    res.text()
}

/// The pushes (type, callId) FCM got, and to which token.
fn pushes(stubs: &phone_common::Stubs) -> Vec<(String, String, String)> {
    stubs
        .pushes()
        .iter()
        .map(|message: &Value| {
            (
                message["token"].as_str().unwrap_or_default().to_string(),
                message["data"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["data"]["callId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn push(token: &str, kind: &str, id: i64) -> (String, String, String) {
    (token.to_string(), kind.to_string(), id.to_string())
}

const HANGUP: &str = "<Hangup/>";

fn assistant(base: &str, id: i64) -> String {
    format!(
        "<Connect action=\"{base}/after-ai?call={id}\"><AIAssistant id=\"assistant-100\"/></Connect>"
    )
}

#[test_db::test(seed)]
async fn an_answered_call_hangs_up_and_logs_its_talk(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    add_phone(&pool, 22, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    inbound(&server, "v3:a-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:a-1").await;
    leg(&server, id, Some(960_021), "leg-1", "initiated").await;
    leg(&server, id, Some(960_021), "leg-1", "ringing").await;
    leg(&server, id, Some(960_022), "leg-2", "ringing").await;
    leg(&server, id, Some(960_021), "leg-1", "in-progress").await;
    let (status, user, device, answered, ended, _) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), user, device, answered, ended),
        ("in_progress", Some(100), Some(960_021), true, false)
    );
    // A late "ringing" never moves a leg back.
    leg(&server, id, Some(960_021), "leg-1", "ringing").await;
    let legs: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT CAST(device_id AS SIGNED), leg_call_sid, status FROM phone_call_legs
          WHERE call_id = ? ORDER BY device_id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        legs,
        [
            (960_021, "leg-1".into(), "in-progress".into()),
            (960_022, "leg-2".into(), "ringing".into())
        ]
    );
    // The talk ends: hang up; answered, with its talk time.
    sql(&pool, &format!("UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(3) - INTERVAL 75 SECOND WHERE id = {id}")).await;
    let answer = post(
        &server,
        &format!("dial-result?call={id}"),
        &[
            ("CallSid", "v3:a-1"),
            ("CallStatus", "in-progress"),
            ("DialCallStatus", "completed"),
            ("DialCallSid", "leg-1"),
            ("DialCallDuration", "75"),
        ],
    )
    .await;
    assert_eq!(answer, texml_doc(HANGUP));
    let (status, user, _, _, ended, talk) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), user, ended),
        ("answered", Some(100), true)
    );
    assert!(
        talk.is_some_and(|secs| (75..=78).contains(&secs)),
        "{talk:?}"
    );
    // Telnyx's status callback afterwards changes nothing.
    post(
        &server,
        "call-status",
        &[
            ("CallSid", "v3:a-1"),
            ("CallStatus", "completed"),
            ("CallDuration", "80"),
        ],
    )
    .await;
    assert_eq!(state(&pool, id).await.0, "answered");
    assert!(stubs.pushes().is_empty());
}

#[test_db::test(seed)]
async fn an_answer_without_its_leg_callback_still_counts(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    inbound(&server, "v3:a-2", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:a-2").await;
    // Only "initiated" came for the leg before the Dial ended.
    leg(&server, id, Some(960_021), "leg-9", "initiated").await;
    post(
        &server,
        &format!("dial-result?call={id}"),
        &[
            ("CallSid", "v3:a-2"),
            ("DialCallStatus", "completed"),
            ("DialCallSid", "leg-9"),
            ("DialCallDuration", "40"),
        ],
    )
    .await;
    let (status, user, device, answered, ended, talk) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), user, device, answered, ended),
        ("answered", Some(100), Some(960_021), true, true)
    );
    // Telnyx's own talk time, not answer to our hearing of the end.
    assert_eq!(talk, Some(40));
    // An absurd duration is held to a day, and answered no earlier than the
    // call started (it never panics or back-dates).
    inbound(&server, "v3:a-3", "+13175550199", "+13175550151").await;
    let absurd = call_id(&pool, "v3:a-3").await;
    post(
        &server,
        &format!("dial-result?call={absurd}"),
        &[
            ("CallSid", "v3:a-3"),
            ("DialCallStatus", "completed"),
            ("DialCallDuration", "9223372036854775807"),
        ],
    )
    .await;
    let (status, _, _, answered, ended, talk) = state(&pool, absurd).await;
    assert_eq!(
        (status.as_str(), answered, ended, talk),
        ("answered", true, true, Some(86_400))
    );
    let early: bool =
        sqlx::query_scalar("SELECT answered_at < started_at FROM phone_calls WHERE id = ?")
            .bind(absurd)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!early);
}

#[test_db::test(seed)]
async fn a_long_call_with_a_customer_moves_the_deal_and_stops_texts(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    sql(&pool, "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100").await;
    sql(
        &pool,
        "INSERT INTO customers (id, name, phone, company_id, source, sales_rep) VALUES
           (930001, 'Tanya Smith', '317-555-0142', 100, 'leads', 100),
           (930002, 'Uma Short', '317-555-0143', 100, 'leads', 100)",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id) VALUES
           (930101, 930001, 'Tanya kitchen', 'New', 100, 3, 100),
           (930102, 930002, 'Uma vanity', 'New', 100, 4, 100)",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO sms_flow_enrollments (id, flow_id, company_id, customer_phone_digits, customer_id, user_id, status, anchor_at) VALUES
           (930201, 1, 100, 3175550142, 930001, 100, 'active', UTC_TIMESTAMP()),
           (930202, 1, 100, 3175550143, 930002, 100, 'active', UTC_TIMESTAMP())",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    // Two minutes with Tanya.
    inbound(&server, "v3:t-1", "+13175550142", "+13175550151").await;
    let id = call_id(&pool, "v3:t-1").await;
    leg(&server, id, Some(960_021), "leg-t1", "in-progress").await;
    sql(&pool, &format!("UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(3) - INTERVAL 120 SECOND WHERE id = {id}")).await;
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:t-1"), ("CallStatus", "completed")],
    )
    .await;
    let (status, _, _, _, _, talk) = state(&pool, id).await;
    assert_eq!(status, "answered");
    assert!(talk.is_some_and(|secs| secs >= 120));
    let deal: (i32, String, i32, Option<i32>) =
        sqlx::query_as("SELECT list_id, status, position, is_won FROM deals WHERE id = 930101")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deal, (101, "Quoted".to_string(), 0, None));
    let stages: Vec<(i32, bool)> = sqlx::query_as(
        "SELECT list_id, exited_at IS NULL FROM deal_stage_history WHERE deal_id = 930101 ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stages, [(101, true)]);
    // Thirty seconds with Uma: she called, so (as with CloudTalk) her deal
    // leaves its first stage too and her texts stop.
    inbound(&server, "v3:u-1", "+13175550143", "+13175550151").await;
    let id = call_id(&pool, "v3:u-1").await;
    leg(&server, id, Some(960_021), "leg-u1", "in-progress").await;
    sql(&pool, &format!("UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(3) - INTERVAL 30 SECOND WHERE id = {id}")).await;
    leg(&server, id, Some(960_021), "leg-u1", "completed").await;
    assert_eq!(state(&pool, id).await.0, "answered");
    let uma: i32 = sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 930102")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(uma, 101);
    let flows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT id, CAST(status AS CHAR) FROM sms_flow_enrollments WHERE id IN (930201, 930202) ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        flows,
        [
            (930_201, "stopped_by_reply".to_string()),
            (930_202, "stopped_by_reply".to_string())
        ]
    );
}

#[test_db::test(seed)]
async fn nobody_answering_rings_the_cells_then_the_assistant(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    add_phone(&pool, 23, 101, 100).await;
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), cell_fallback = 1",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    inbound(&server, "v3:c-1", "+13175550100", "+13175550152").await;
    let id = call_id(&pool, "v3:c-1").await;
    let no_answer = [
        ("CallSid", "v3:c-1"),
        ("CallStatus", "in-progress"),
        ("DialCallStatus", "no-answer"),
    ];
    let cells = post(&server, &format!("dial-result?call={id}"), &no_answer).await;
    assert_eq!(
        cells,
        texml_doc(&format!(
            "<Dial timeout=\"25\" action=\"{base}/dial-result?call={id}&amp;cell=1\" \
             record=\"record-from-answer-dual\" recordingStatusCallback=\"{base}/recording?call={id}&amp;kind=call\">\
             <Number url=\"{base}/cell-screen?call={id}&amp;user=100\">+13175550177</Number></Dial>"
        ))
    );
    assert_eq!(state(&pool, id).await.0, "ringing");
    // The cell answers: "press 1".
    let screen = post(
        &server,
        &format!("cell-screen?call={id}&user=100"),
        &[("CallSid", "cell-leg-1"), ("To", "+13175550177")],
    )
    .await;
    assert_eq!(
        screen,
        texml_doc(&format!(
            "<Gather numDigits=\"1\" timeout=\"8\" action=\"{base}/cell-screen-result?call={id}&amp;user=100\">\
             <Say>Granite Phone: a call from Alice Lead. Press 1 to answer.</Say></Gather><Hangup/>"
        ))
    );
    // A 1 from a leg that is not Rita's cell is no answer of hers.
    let elsewhere = post(
        &server,
        &format!("cell-screen-result?call={id}&user=100"),
        &[
            ("CallSid", "cell-leg-x"),
            ("To", "+13175550188"),
            ("Digits", "1"),
        ],
    )
    .await;
    assert_eq!(elsewhere, texml_doc(HANGUP));
    assert_eq!(state(&pool, id).await.0, "ringing");
    let connect = post(
        &server,
        &format!("cell-screen-result?call={id}&user=100"),
        &[
            ("CallSid", "cell-leg-1"),
            ("To", "+13175550177"),
            ("Digits", "1"),
        ],
    )
    .await;
    assert_eq!(connect, texml_doc(""));
    assert_eq!(
        state(&pool, id).await,
        (
            "in_progress".to_string(),
            Some(100),
            None,
            true,
            false,
            None
        )
    );
    let done = post(
        &server,
        &format!("dial-result?call={id}&cell=1"),
        &[("CallSid", "v3:c-1"), ("DialCallStatus", "completed")],
    )
    .await;
    assert_eq!(done, texml_doc(HANGUP));
    assert_eq!(state(&pool, id).await.0, "answered");
    assert!(stubs.pushes().is_empty());
    // Next call: the cell picks up but never presses 1, so the AI
    // Assistant takes it; everyone it rang gets a missed-call push.
    inbound(&server, "v3:c-2", "+13175550100", "+13175550152").await;
    let id = call_id(&pool, "v3:c-2").await;
    post(
        &server,
        &format!("dial-result?call={id}"),
        &[("CallSid", "v3:c-2"), ("DialCallStatus", "no-answer")],
    )
    .await;
    let hung_up = post(
        &server,
        &format!("cell-screen-result?call={id}&user=100"),
        &[("CallSid", "cell-leg-2"), ("Digits", "5")],
    )
    .await;
    assert_eq!(hung_up, texml_doc(HANGUP));
    let ai = post(
        &server,
        &format!("dial-result?call={id}&cell=1"),
        &[("CallSid", "v3:c-2"), ("DialCallStatus", "completed")],
    )
    .await;
    assert_eq!(ai, texml_doc(&assistant(&base, id)));
    assert_eq!(state(&pool, id).await.0, "missed");
    assert_eq!(
        pushes(&stubs),
        [
            push("push-21", "missed_call", id),
            push("push-23", "missed_call", id)
        ]
    );
}

#[test_db::test(seed)]
async fn declining_sends_the_call_to_the_no_answer_action(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    add_phone(&pool, 22, 100, 100).await;
    // A decline skips the cell fallback.
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), cell_fallback = 1",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    inbound(&server, "v3:d-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:d-1").await;
    leg(&server, id, Some(960_021), "leg-d1", "ringing").await;
    leg(&server, id, Some(960_022), "leg-d2", "ringing").await;
    server
        .post("/v1/phone/calls/decline")
        .add_header(HEADER, token(21))
        .json(&json!({ "telnyxCallId": "leg-d1" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // TeXML's update by the call alone: no account id needed.
    let redirect = format!("{base}/dial-result?call={id}&declined=1");
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/v3:d-1/update".to_string(),
            json!({ "Url": redirect, "Method": "POST" })
        )]
    );
    // Telnyx follows the redirect.
    let answer = post(
        &server,
        &format!("dial-result?call={id}&declined=1"),
        &[("CallSid", "v3:d-1"), ("CallStatus", "in-progress")],
    )
    .await;
    assert_eq!(answer, texml_doc(&assistant(&base, id)));
    assert_eq!(state(&pool, id).await.0, "declined");
    assert_eq!(
        pushes(&stubs),
        [
            push("push-21", "missed_call", id),
            push("push-22", "missed_call", id)
        ]
    );
}

#[test_db::test(seed)]
async fn the_assistant_falls_back_to_voicemail_or_takes_a_message(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    sql(
        &pool,
        "UPDATE telnyx_numbers SET greeting = 'Leave a message for Rita.' WHERE id = 9821",
    )
    .await;
    push_only_phone(&pool, 21, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    inbound(&server, "v3:ai-1", "+13175550199", "+13175550151").await;
    let first = call_id(&pool, "v3:ai-1").await;
    let voicemail = post(
        &server,
        &format!("after-ai?call={first}"),
        &[
            ("CallSid", "v3:ai-1"),
            ("CallStatus", "conversation_start_failed"),
            ("Reason", "invalid_request"),
        ],
    )
    .await;
    assert_eq!(
        voicemail,
        texml_doc(&format!(
            "<Say>Leave a message for Rita.</Say><Record maxLength=\"120\" playBeep=\"true\" \
             action=\"{base}/voicemail-done?call={first}\" \
             recordingStatusCallback=\"{base}/recording?call={first}&amp;kind=voicemail\"/>"
        ))
    );
    assert_eq!(state(&pool, first).await.0, "missed");
    // The assistant talked with the caller: an AI message.
    inbound(&server, "v3:ai-2", "+13175550199", "+13175550151").await;
    let second = call_id(&pool, "v3:ai-2").await;
    let done = post(
        &server,
        &format!("after-ai?call={second}"),
        &[
            ("CallSid", "v3:ai-2"),
            ("CallStatus", "completed"),
            ("Reason", "hangup"),
        ],
    )
    .await;
    assert_eq!(done, texml_doc(HANGUP));
    let (status, _, _, answered, ended, _) = state(&pool, second).await;
    assert_eq!(
        (status.as_str(), answered, ended),
        ("ai_message", false, true)
    );
    assert_eq!(
        pushes(&stubs),
        [
            push("push-21", "missed_call", first),
            push("push-21", "missed_call", second),
            push("push-21", "ai_message", second)
        ]
    );
}

#[test_db::test(seed)]
async fn a_voicemail_ends_the_call(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    push_only_phone(&pool, 21, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    inbound(&server, "v3:vm-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:vm-1").await;
    let left = [
        ("CallSid", "v3:vm-1"),
        ("RecordingSid", "rec-vm-1"),
        ("RecordingDuration", "12"),
    ];
    let done = post(&server, &format!("voicemail-done?call={id}"), &left).await;
    assert_eq!(done, texml_doc(HANGUP));
    // Sent twice: one push.
    post(&server, &format!("voicemail-done?call={id}"), &left).await;
    let (status, _, _, _, ended, _) = state(&pool, id).await;
    assert_eq!((status.as_str(), ended), ("voicemail", true));
    // Nothing recorded: still a missed call.
    inbound(&server, "v3:vm-2", "+13175550199", "+13175550151").await;
    let empty = call_id(&pool, "v3:vm-2").await;
    post(
        &server,
        &format!("voicemail-done?call={empty}"),
        &[("CallSid", "v3:vm-2"), ("RecordingDuration", "0")],
    )
    .await;
    assert_eq!(state(&pool, empty).await.0, "missed");
    assert_eq!(
        pushes(&stubs),
        [
            push("push-21", "missed_call", id),
            push("push-21", "voicemail", id),
            push("push-21", "missed_call", empty)
        ]
    );
}

#[test_db::test(seed)]
async fn a_caller_who_hangs_up_while_it_rings_is_a_missed_call(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    inbound(&server, "v3:h-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:h-1").await;
    let hung_up = [("CallSid", "v3:h-1"), ("CallStatus", "completed")];
    post(&server, "call-status", &hung_up).await;
    post(&server, "call-status", &hung_up).await;
    let (status, _, _, answered, ended, talk) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), answered, ended, talk),
        ("canceled", false, true, None)
    );
    assert_eq!(pushes(&stubs), [push("push-21", "missed_call", id)]);
    // The Dial's action after the caller left just hangs up.
    let answer = post(
        &server,
        &format!("dial-result?call={id}"),
        &[
            ("CallSid", "v3:h-1"),
            ("CallStatus", "completed"),
            ("DialCallStatus", "canceled"),
        ],
    )
    .await;
    assert_eq!(answer, texml_doc(HANGUP));
    assert_eq!(stubs.pushes().len(), 1);
    // Callbacks about unknown calls are taken and ignored.
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:nope"), ("CallStatus", "completed")],
    )
    .await;
    post(
        &server,
        "leg-status?call=999999&device=960021",
        &[("CallSid", "leg-x"), ("CallStatus", "ringing")],
    )
    .await;
}

#[test_db::test(seed)]
async fn only_a_phone_the_call_rang_answers_it(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 21, 100, 100).await;
    // Mark's phone (the call to Rita's direct line never rings it) and a
    // phone of company 101.
    add_phone(&pool, 24, 101, 100).await;
    add_phone(&pool, 29, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    inbound(&server, "v3:o-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:o-1").await;
    leg(&server, id, Some(960_029), "leg-o1", "in-progress").await;
    leg(&server, id, Some(960_024), "leg-o2", "in-progress").await;
    leg(&server, id, None, "leg-o3", "in-progress").await;
    // The legs are noted without the phones; nobody answered.
    assert_eq!(
        state(&pool, id).await,
        ("ringing".into(), None, None, false, false, None)
    );
    let stored: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT CAST(device_id AS SIGNED) FROM phone_call_legs
          WHERE leg_call_sid IN ('leg-o1', 'leg-o2', 'leg-o3') ORDER BY leg_call_sid",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored, [None, None, None]);
    // Rita's phone, which it rang, does.
    leg(&server, id, Some(960_021), "leg-o4", "in-progress").await;
    let (status, user, device, answered, _, _) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), user, device, answered),
        ("in_progress", Some(100), Some(960_021), true)
    );
}

#[test_db::test(seed)]
async fn internal_errors_redirect_to_the_fallback(pool: MySqlPool) {
    enable_calling(
        &pool,
        100,
        "fallback_texml_url = 'https://bin.test/fallback?x=1&y=2'",
    )
    .await;
    let (server, _) = phone_server_with(pool.clone(), |state| {
        state.with_phone_config(PhoneConfig {
            public_api_url: Arc::from(PUBLIC_API),
            fallback_texml_url: Some(Arc::from("https://bin.test/global")),
            ..PhoneConfig::default()
        })
    })
    .await;
    let unknown = [("CallSid", "v3:lost"), ("DialCallStatus", "no-answer")];
    let company_bin = texml_doc("<Redirect>https://bin.test/fallback?x=1&amp;y=2</Redirect>");
    assert_eq!(
        post(&server, "dial-result?call=999999", &unknown).await,
        company_bin
    );
    assert_eq!(
        post(&server, "after-ai?call=999999", &unknown).await,
        company_bin
    );
    // The company has none: TELNYX_FALLBACK_TEXML_URL.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET fallback_texml_url = NULL WHERE company_id = 100",
    )
    .await;
    assert_eq!(
        post(&server, "dial-result?call=999999", &unknown).await,
        texml_doc("<Redirect>https://bin.test/global</Redirect>")
    );
    // Neither: an apology.
    let (bare, _) = phone_server(pool.clone()).await;
    assert_eq!(
        post(&bare, "dial-result?call=999999", &unknown).await,
        texml_doc(
            "<Say>Sorry, we cannot take your call right now. Please call again later.</Say><Hangup/>"
        )
    );
    // Errors of the request itself are not hidden.
    post_texml(&server, 100, "dial-result", &unknown)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
