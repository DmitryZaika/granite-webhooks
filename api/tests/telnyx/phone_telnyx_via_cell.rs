//! The calls our server places (`routes/telnyx_webhooks/via_cell.rs`,
//! `misc.rs`): Call via my cell ("press 1", then the customer, with the
//! recording notice when outgoing calls are recorded), test calls and cell
//! verification codes.
//!
//! Fixture phones use n = 61..=80; numbers 9861+; calls 980950+.

use crate::phone_common;

use phone_common::{
    HEADER, add_call, add_number, add_phone, assign, enable_calling, phone_server, post_texml,
    set_settings, sql, texml_doc, token, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita's direct line, her default caller ID, and her verified cell.
async fn rita(pool: &MySqlPool) {
    add_number(pool, 9861, 100, "+13175550151", "direct", "Rita direct").await;
    assign(pool, 9861, 100, true, true).await;
    set_settings(
        pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), default_caller_number_id = 9861",
    )
    .await;
    add_phone(pool, 61, 100, 100).await;
}

/// Rita starts a Call via my cell to Alice; our call id.
async fn call_alice(server: &axum_test::TestServer) -> i64 {
    let res = server
        .post("/v1/phone/calls/via-cell")
        .add_header(HEADER, token(61))
        .json(&json!({ "to": "3175550100" }))
        .await;
    res.assert_status(axum::http::StatusCode::ACCEPTED);
    res.json::<Value>()["callId"].as_i64().unwrap()
}

async fn post(server: &axum_test::TestServer, route: &str, form: &[(&str, &str)]) -> String {
    let res = post_texml(server, 100, route, form).await;
    res.assert_status_ok();
    res.text()
}

/// A call's (status, answered, ended, talk time).
async fn state(pool: &MySqlPool, id: i64) -> (String, bool, bool, Option<i32>) {
    sqlx::query_as(
        "SELECT CAST(status AS CHAR), answered_at IS NOT NULL, ended_at IS NOT NULL, duration_secs
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

const HANGUP: &str = "<Hangup/>";

#[test_db::test(seed)]
async fn call_via_my_cell_asks_then_dials_the_customer(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let id = call_alice(&server).await;
    // Rita's cell answers.
    let prompt = post(
        &server,
        &format!("via-cell?call={id}"),
        &[
            ("CallSid", "v3:sid-1"),
            ("CallStatus", "in-progress"),
            ("To", "+13175550177"),
        ],
    )
    .await;
    assert_eq!(
        prompt,
        texml_doc(&format!(
            "<Gather numDigits=\"1\" timeout=\"8\" action=\"{base}/via-cell-gather?call={id}\">\
             <Say>Press 1 to call Alice Lead.</Say></Gather><Hangup/>"
        ))
    );
    // 1: Alice is dialed from Rita's line (not recorded: incoming only).
    let dial = post(
        &server,
        &format!("via-cell-gather?call={id}"),
        &[("CallSid", "v3:sid-1"), ("Digits", "1")],
    )
    .await;
    assert_eq!(
        dial,
        texml_doc(&format!(
            "<Dial callerId=\"+13175550151\"><Number statusCallback=\"{base}/leg-status?call={id}\" \
             statusCallbackEvent=\"answered completed\">+13175550100</Number></Dial>"
        ))
    );
    // Alice answers; after 95 seconds she hangs up.
    let leg = |status: &'static str| {
        [
            ("CallSid", "leg-alice"),
            ("CallStatus", status),
            ("AccountSid", "acct-100"),
        ]
    };
    post(
        &server,
        &format!("leg-status?call={id}"),
        &leg("in-progress"),
    )
    .await;
    assert_eq!(
        state(&pool, id).await,
        ("in_progress".into(), true, false, None)
    );
    sql(&pool, &format!("UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(3) - INTERVAL 95 SECOND WHERE id = {id}")).await;
    post(&server, &format!("leg-status?call={id}"), &leg("completed")).await;
    let (status, _, ended, talk) = state(&pool, id).await;
    assert_eq!((status.as_str(), ended), ("answered", true));
    assert!(
        talk.is_some_and(|secs| (95..=98).contains(&secs)),
        "{talk:?}"
    );
    // Telnyx's status callback of the cell call changes nothing more.
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:sid-1"), ("CallStatus", "completed")],
    )
    .await;
    let call: Value = server
        .get(&format!("/v1/phone/calls/{id}"))
        .add_header(HEADER, token(61))
        .await
        .json();
    assert_eq!(
        (&call["viaCell"], &call["status"], &call["userId"]),
        (&json!(true), &json!("answered"), &json!(100))
    );
}

#[test_db::test(seed)]
async fn the_customer_hears_the_notice_when_outgoing_calls_are_recorded(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'both'").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let id = call_alice(&server).await;
    let dial = post(
        &server,
        &format!("via-cell-gather?call={id}"),
        &[("CallSid", "v3:sid-1"), ("Digits", "1")],
    )
    .await;
    assert_eq!(
        dial,
        texml_doc(&format!(
            "<Dial callerId=\"+13175550151\" record=\"record-from-answer-dual\" \
             recordingStatusCallback=\"{base}/recording?call={id}&amp;kind=call\">\
             <Number url=\"{base}/notice\" statusCallback=\"{base}/leg-status?call={id}\" \
             statusCallbackEvent=\"answered completed\">+13175550100</Number></Dial>"
        ))
    );
    assert_eq!(
        post(&server, "notice", &[("CallSid", "leg-alice")]).await,
        texml_doc("<Say>This call may be recorded.</Say>")
    );
}

#[test_db::test(seed)]
async fn call_via_my_cell_ends_without_a_1_or_an_answer(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    // Rita presses 2.
    let first = call_alice(&server).await;
    let answer = post(
        &server,
        &format!("via-cell-gather?call={first}"),
        &[("CallSid", "v3:sid-1"), ("Digits", "2")],
    )
    .await;
    assert_eq!(answer, texml_doc(HANGUP));
    assert_eq!(
        state(&pool, first).await,
        ("canceled".into(), false, true, None)
    );
    // Alice's line is busy.
    let second = call_alice(&server).await;
    post(
        &server,
        &format!("via-cell-gather?call={second}"),
        &[("CallSid", "v3:sid-2"), ("Digits", "1")],
    )
    .await;
    post(
        &server,
        &format!("leg-status?call={second}"),
        &[("CallSid", "leg-busy"), ("CallStatus", "busy")],
    )
    .await;
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:sid-2"), ("CallStatus", "completed")],
    )
    .await;
    assert_eq!(
        state(&pool, second).await,
        ("busy".into(), false, true, None)
    );
    // Rita's cell never answers.
    let third = call_alice(&server).await;
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:sid-3"), ("CallStatus", "no-answer")],
    )
    .await;
    assert_eq!(
        state(&pool, third).await,
        ("canceled".into(), false, true, None)
    );
    // An incoming call is no Call via my cell.
    add_call(
        &pool,
        980_950,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550199",
        "2026-10-18 10:00:00",
        "number_id = 9861",
    )
    .await;
    assert_eq!(
        post(&server, "via-cell?call=980950", &[("CallSid", "v3:other")]).await,
        texml_doc(HANGUP)
    );
}

#[test_db::test(seed)]
async fn a_final_status_before_the_call_sid_still_ends_the_call(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    // Telnyx's status callback beats its answer to our request: the call
    // has no sid yet, but the callback names our call.
    let id = call_alice(&server).await;
    sql(
        &pool,
        &format!("UPDATE phone_calls SET telnyx_call_sid = NULL WHERE id = {id}"),
    )
    .await;
    post(
        &server,
        &format!("call-status?call={id}"),
        &[("CallSid", "v3:early"), ("CallStatus", "no-answer")],
    )
    .await;
    assert_eq!(
        state(&pool, id).await,
        ("canceled".into(), false, true, None)
    );
    let sid: Option<String> =
        sqlx::query_scalar("SELECT telnyx_call_sid FROM phone_calls WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sid.as_deref(), Some("v3:early"));
    // A callback naming our call with another call's sid changes nothing.
    let other = call_alice(&server).await;
    post(
        &server,
        &format!("call-status?call={other}"),
        &[("CallSid", "v3:not-it"), ("CallStatus", "failed")],
    )
    .await;
    assert_eq!(state(&pool, other).await.0, "ringing");
}

#[test_db::test(seed)]
async fn a_customer_leg_that_completed_was_answered(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let id = call_alice(&server).await;
    post(
        &server,
        &format!("via-cell-gather?call={id}"),
        &[("CallSid", "v3:sid-1"), ("Digits", "1")],
    )
    .await;
    // Alice's leg reports `completed` (with its talk time) before any
    // `in-progress`: she answered and talked 95 seconds.
    post(
        &server,
        &format!("leg-status?call={id}"),
        &[
            ("CallSid", "leg-alice"),
            ("CallStatus", "completed"),
            ("CallDuration", "95"),
        ],
    )
    .await;
    let (status, answered, ended, talk) = state(&pool, id).await;
    assert_eq!(
        (status.as_str(), answered, ended, talk),
        ("answered", true, true, Some(95))
    );
    // An absurd duration is held to a day.
    let next = call_alice(&server).await;
    post(
        &server,
        &format!("via-cell-gather?call={next}"),
        &[("CallSid", "v3:sid-2"), ("Digits", "1")],
    )
    .await;
    post(
        &server,
        &format!("leg-status?call={next}"),
        &[
            ("CallSid", "leg-bob"),
            ("CallStatus", "completed"),
            ("CallDuration", "99999999999999"),
        ],
    )
    .await;
    assert_eq!(state(&pool, next).await.3, Some(86_400));
}

#[test_db::test(seed)]
async fn test_calls_say_hello_and_stay_out_of_recents(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let res = server
        .post("/v1/phone/calls/test")
        .add_header(HEADER, token(61))
        .await;
    res.assert_status(axum::http::StatusCode::ACCEPTED);
    let id = res.json::<Value>()["callId"].as_i64().unwrap();
    let hello = post(
        &server,
        &format!("test-call?call={id}"),
        &[("CallSid", "v3:sid-1")],
    )
    .await;
    assert_eq!(
        hello,
        texml_doc(
            "<Say>This is a Granite Phone test call. Your phone can take calls. Goodbye.</Say><Hangup/>"
        )
    );
    assert_eq!(
        state(&pool, id).await,
        ("in_progress".into(), true, false, None)
    );
    post(
        &server,
        "call-status",
        &[("CallSid", "v3:sid-1"), ("CallStatus", "completed")],
    )
    .await;
    let (status, _, ended, _) = state(&pool, id).await;
    assert_eq!((status.as_str(), ended), ("answered", true));
    let recents: Value = server
        .get("/v1/phone/calls")
        .add_header(HEADER, token(61))
        .await
        .json();
    assert_eq!(recents["calls"], json!([]));
    // Another call is no test call.
    add_call(
        &pool,
        980_951,
        100,
        None,
        "inbound",
        "ringing",
        "+13175550199",
        "2026-10-18 10:00:00",
        "number_id = 9861",
    )
    .await;
    assert_eq!(
        post(&server, "test-call?call=980951", &[("CallSid", "v3:other")]).await,
        texml_doc(HANGUP)
    );
    assert_eq!(state(&pool, 980_951).await.0, "ringing");
}

#[test_db::test(seed)]
async fn cell_codes_are_read_to_the_cell_being_verified(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita(&pool).await;
    // Olga (company 101) has a code pending in her company.
    set_settings(
        &pool,
        106,
        101,
        "cell_pending_number = '+13175550177', cell_code = '123456',
         cell_code_expires_at = UTC_TIMESTAMP() + INTERVAL 5 MINUTE",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    server
        .post("/v1/phone/cell/verify")
        .add_header(HEADER, token(61))
        .json(&json!({ "cellNumber": "317-555-0178" }))
        .await
        .assert_status(axum::http::StatusCode::ACCEPTED);
    let code: String =
        sqlx::query_scalar("SELECT cell_code FROM phone_user_settings WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    let spoken = code
        .chars()
        .map(String::from)
        .collect::<Vec<_>>()
        .join(", ");
    let read = post(
        &server,
        "cell-code?user=100",
        &[("CallSid", "v3:sid-1"), ("To", "+13175550178")],
    )
    .await;
    assert_eq!(
        read,
        texml_doc(&format!(
            "<Say>Your Granite Phone code is {spoken}.</Say><Pause length=\"1\"/>\
             <Say>Again, your code is {spoken}.</Say><Hangup/>"
        ))
    );
    let none = texml_doc(
        "<Say>This code is no longer valid. Ask for a new one in the Granite Phone app. Goodbye.</Say><Hangup/>",
    );
    // Not the cell being verified (Rita's verified cell is another one), or
    // no number named at all.
    assert_eq!(
        post(&server, "cell-code?user=100", &[("To", "+13175550177")]).await,
        none
    );
    assert_eq!(
        post(&server, "cell-code?user=100", &[("CallSid", "v3:sid-9")]).await,
        none
    );
    // Another company's user.
    assert_eq!(
        post(&server, "cell-code?user=106", &[("To", "+13175550177")]).await,
        none
    );
    // Expired.
    sql(&pool, "UPDATE phone_user_settings SET cell_code_expires_at = UTC_TIMESTAMP() - INTERVAL 1 SECOND WHERE user_id = 100").await;
    assert_eq!(
        post(&server, "cell-code?user=100", &[("To", "+13175550178")]).await,
        none
    );
}
