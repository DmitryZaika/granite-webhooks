//! The app's outgoing calls through Call Control
//! (`routes/telnyx_webhooks/outbound.rs`): Telnyx's signature, the phone's
//! own call signature (`X-Granite-Call-Auth`), approving a parked call (the
//! exact transfer), refusals (hung up, logged failed), emergency codes,
//! limits, duplicates and the call's later events.
//!
//! Fixture phones use n = 41..=60; numbers 9841+; customers 930101+.

mod common;
mod phone_common;

use axum::body::Bytes;
use axum::http::StatusCode;
use phone_common::{
    Signer, Stubs, add_number, add_phone, assign, call_auth, enable_calling, hook_path,
    phone_server, phone_server_with, revoke_session, signed_config, sql, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita's direct line and the sales line (both hers to call from), Mark's
/// direct line, and Olga's line in company 101.
async fn lines(pool: &MySqlPool) {
    add_number(pool, 9841, 100, "+13175550151", "direct", "Rita direct").await;
    add_number(pool, 9842, 100, "+13175550152", "shared", "Sales").await;
    add_number(pool, 9843, 100, "+13175550154", "direct", "Mark direct").await;
    add_number(pool, 9844, 101, "+14635550201", "direct", "Olga direct").await;
    assign(pool, 9841, 100, true, true).await;
    assign(pool, 9842, 100, true, true).await;
    assign(pool, 9843, 101, true, true).await;
    assign(pool, 9844, 106, true, true).await;
}

/// A server checking Telnyx's signatures with `signer`'s key.
async fn signed_server(pool: &MySqlPool, signer: &Signer) -> (axum_test::TestServer, Stubs) {
    let key = signer.public_key.clone();
    phone_server_with(pool.clone(), move |state| {
        state.with_phone_config(signed_config(&key))
    })
    .await
}

/// A Call Control event that happened now.
fn event(id: &str, kind: &str, payload: &Value) -> Value {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    json!({ "data": {
        "record_type": "event", "id": id, "event_type": kind,
        "occurred_at": now, "payload": payload },
        "meta": { "attempt": 1 } })
}

/// The app's parked call `ccid` from `from` to `to`, with these custom SIP
/// headers.
fn parked(ccid: &str, from: &str, to: &str, headers: &[(&str, &str)]) -> Value {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    json!({
        "call_control_id": ccid, "call_leg_id": format!("leg-{ccid}"),
        "call_session_id": format!("session-{ccid}"), "connection_id": "conn-100",
        "from": from, "to": to, "direction": "outgoing", "state": "parked",
        "custom_headers": headers })
}

/// Now, in Unix seconds.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Phone `n`'s parked call `ccid` to `to` with caller ID `caller`, as the
/// app places it: `from` is the caller ID, and the headers name the phone
/// (device 960000 + n) and sign the call with its call key now
/// (`phone_common::call_auth`).
fn app_call(ccid: &str, n: u32, to: &str, caller: &str) -> Value {
    let device = (960_000 + n).to_string();
    let auth = call_auth(n, &device, to, caller, now());
    parked(
        ccid,
        caller,
        to,
        &[
            ("X-Granite-Device-Id", &device),
            ("X-Granite-Caller-Id", caller),
            ("X-Granite-Call-Auth", &auth),
        ],
    )
}

/// Posts `body` as Telnyx does, signed now by `signer` (unsigned when none).
async fn send(
    server: &axum_test::TestServer,
    signer: Option<&Signer>,
    body: &Value,
) -> axum_test::TestResponse {
    let bytes = serde_json::to_vec(body).unwrap();
    let mut request = server
        .post(&hook_path(100, "outbound"))
        .content_type("application/json")
        .bytes(Bytes::from(bytes.clone()));
    if let Some(signer) = signer {
        for (name, value) in signer.headers(&bytes) {
            request = request.add_header(name, value);
        }
    }
    request.await
}

/// Our call of the app's leg `ccid`.
async fn call_of(pool: &MySqlPool, ccid: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM phone_calls WHERE telnyx_call_control_id = ?",
    )
    .bind(ccid)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A call's (status, approved, ended, talk time).
async fn state(pool: &MySqlPool, id: i64) -> (String, bool, bool, Option<i32>) {
    sqlx::query_as(
        "SELECT CAST(status AS CHAR), approved <> 0, ended_at IS NOT NULL, duration_secs
           FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn call_control_events_must_be_signed(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let signer = Signer::new();
    let (server, _) = signed_server(&pool, &signer).await;
    let body = event(
        "evt-0",
        "call.speak.ended",
        &json!({ "connection_id": "conn-100" }),
    );
    send(&server, None, &body)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    send(&server, Some(&Signer::new()), &body)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // Signed ten minutes ago: a replay.
    let bytes = serde_json::to_vec(&body).unwrap();
    let old = (chrono::Utc::now().timestamp() - 600).to_string();
    server
        .post(&hook_path(100, "outbound"))
        .bytes(Bytes::from(bytes.clone()))
        .add_header("telnyx-signature-ed25519", signer.sign(&old, &bytes))
        .add_header("telnyx-timestamp", old)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    send(&server, Some(&signer), &body).await.assert_status_ok();
    // Another connection than the company's credential connection.
    let other = event(
        "evt-1",
        "call.speak.ended",
        &json!({ "connection_id": "conn-999" }),
    );
    send(&server, Some(&signer), &other)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    // Signed, but not an event.
    let mut garbage = server
        .post(&hook_path(100, "outbound"))
        .bytes(Bytes::from_static(b"not json"));
    for (name, value) in signer.headers(b"not json") {
        garbage = garbage.add_header(name, value);
    }
    garbage.await.assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/webhooks/telnyx/tok00000000000000000000000000999/outbound")
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Without TELNYX_PUBLIC_KEY nothing can be checked: refused, unless the
    // server allows unsigned events (PHONE_ALLOW_UNSIGNED_WEBHOOKS=1, the
    // fixture server's local-testing switch).
    let (unset, _) = phone_server_with(pool.clone(), |state| {
        state.with_phone_config(api::phone::PhoneConfig {
            public_api_url: std::sync::Arc::from(phone_common::PUBLIC_API),
            ..api::phone::PhoneConfig::default()
        })
    })
    .await;
    let refused = send(
        &unset,
        None,
        &event("evt-2", "call.speak.ended", &json!({})),
    )
    .await;
    refused.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let (open, _) = phone_server(pool.clone()).await;
    send(&open, None, &event("evt-3", "call.speak.ended", &json!({})))
        .await
        .assert_status_ok();
    let stored: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM phone_call_events WHERE source = 'call_control'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    // The refused event was never stored.
    assert_eq!(stored, 2);
}

#[test_db::test(seed)]
async fn a_parked_call_goes_out_with_the_callers_line(pool: MySqlPool) {
    enable_calling(
        &pool,
        100,
        "recording_mode = 'both', recording_notice_url = 'https://cdn.test/notice.mp3'",
    )
    .await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    sql(&pool, "INSERT INTO customers (id, name, phone, company_id, source) VALUES (930101, 'Vera Customer', '317-555-0142', 100, 'leads')").await;
    sql(&pool, "INSERT INTO deals (id, customer_id, title, status, list_id, position, user_id) VALUES (930201, 930101, 'Vera kitchen', 'New', 100, 0, 100)").await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    let auth = call_auth(41, "960041", "+13175550142", "+13175550152", now());
    let initiated = event(
        "evt-1",
        "call.initiated",
        &parked(
            "v3:app-1",
            "+13175550152",
            "+13175550142",
            &[
                ("X-Granite-Device-Id", "960041"),
                ("x-granite-caller-id", "+13175550152"),
                ("X-Granite-Customer-Id", "930101"),
                ("x-granite-call-auth", &auth),
            ],
        ),
    );
    send(&server, Some(&signer), &initiated)
        .await
        .assert_status_ok();
    let id = call_of(&pool, "v3:app-1").await;
    let transfer = json!({
        "to": "+13175550142", "from": "+13175550152",
        "audio_url": "https://cdn.test/notice.mp3", "record": "record-from-answer",
        "record_channels": "dual", "record_format": "mp3", "timeout_secs": 60,
        "webhook_url": format!("{}/outbound", webhook_base(100)),
        "command_id": format!("approve-{id}") });
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/calls/v3:app-1/actions/transfer".to_string(),
            transfer
        )]
    );
    let row: (String, String, i32, String, Option<i32>, Option<i64>, Option<i32>, Option<String>, bool, Option<String>) =
        sqlx::query_as(
            "SELECT CAST(direction AS CHAR), CAST(status AS CHAR), user_id, other_number, customer_id,
                    CAST(deal_id AS SIGNED), number_id, caller_id, ai_notes <> 0, telnyx_call_session_id
               FROM phone_calls WHERE id = ? AND company_id = 100 AND approved = 1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row,
        (
            "outbound".into(),
            "ringing".into(),
            100,
            "+13175550142".into(),
            Some(930_101),
            Some(930_201),
            Some(9842),
            Some("+13175550152".into()),
            true,
            Some("session-v3:app-1".into())
        )
    );
    // Telnyx sends it again: one transfer.
    send(&server, Some(&signer), &initiated)
        .await
        .assert_status_ok();
    assert_eq!(stubs.telnyx().len(), 1);
    // Vera answers; two minutes later she hangs up.
    let leg = |kind: &str, ccid: &str, extra: Value| {
        let mut payload = json!({ "call_control_id": ccid, "call_session_id": "session-v3:app-1",
                                  "connection_id": "conn-100" });
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        (kind.to_string(), payload)
    };
    // The transfer bridges the legs while Vera's phone still rings: not an
    // answer. Neither is the app's own leg answering.
    let (kind, payload) = leg("call.bridged", "v3:app-1", json!({}));
    send(
        &server,
        Some(&signer),
        &event("evt-bridged", &kind, &payload),
    )
    .await
    .assert_status_ok();
    let (kind, payload) = leg("call.answered", "v3:app-1", json!({}));
    send(&server, Some(&signer), &event("evt-own", &kind, &payload))
        .await
        .assert_status_ok();
    assert_eq!(
        state(&pool, id).await,
        ("ringing".into(), true, false, None)
    );
    let (kind, payload) = leg("call.answered", "v3:vera", json!({}));
    send(&server, Some(&signer), &event("evt-2", &kind, &payload))
        .await
        .assert_status_ok();
    assert_eq!(
        state(&pool, id).await,
        ("in_progress".into(), true, false, None)
    );
    sql(&pool, &format!("UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(3) - INTERVAL 120 SECOND WHERE id = {id}")).await;
    let (kind, payload) = leg(
        "call.hangup",
        "v3:vera",
        json!({ "hangup_cause": "normal_clearing", "hangup_source": "callee" }),
    );
    send(&server, Some(&signer), &event("evt-3", &kind, &payload))
        .await
        .assert_status_ok();
    let (status, _, ended, talk) = state(&pool, id).await;
    assert_eq!((status.as_str(), ended), ("answered", true));
    assert!(talk.is_some_and(|secs| secs >= 120));
    let list: i32 = sqlx::query_scalar("SELECT list_id FROM deals WHERE id = 930201")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(list, 101);
    // The app's leg hangs up after her: nothing changes.
    let (kind, payload) = leg(
        "call.hangup",
        "v3:app-1",
        json!({ "hangup_cause": "normal_clearing", "hangup_source": "caller" }),
    );
    send(&server, Some(&signer), &event("evt-4", &kind, &payload))
        .await
        .assert_status_ok();
    assert_eq!(state(&pool, id).await.0, "answered");
    // Its own SIP user in `from`, signed, without a caller ID header: the
    // default caller ID (Rita's direct line).
    stubs.clear_telnyx();
    let auth = call_auth(41, "960041", "+13175550143", "", now());
    let by_sip = parked(
        "v3:app-2",
        "sip:gencred41@sip.telnyx.com",
        "+13175550143",
        &[
            ("X-Granite-Device-Id", "960041"),
            ("X-Granite-Call-Auth", &auth),
        ],
    );
    send(
        &server,
        Some(&signer),
        &event("evt-5", "call.initiated", &by_sip),
    )
    .await
    .assert_status_ok();
    let second = call_of(&pool, "v3:app-2").await;
    let sent = stubs.telnyx();
    assert_eq!(sent[0].1, "/calls/v3:app-2/actions/transfer");
    assert_eq!(
        (
            &sent[0].2["to"],
            &sent[0].2["from"],
            &sent[0].2["command_id"]
        ),
        (
            &json!("+13175550143"),
            &json!("+13175550151"),
            &json!(format!("approve-{second}"))
        )
    );
}

#[test_db::test(seed)]
async fn refused_calls_are_hung_up_and_logged_failed(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    enable_calling(&pool, 101, "").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 43, 101, 100).await;
    add_phone(&pool, 49, 106, 101).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    // Rita's phone signs the call, but `from` names Mark's SIP user.
    let mut other_sip = app_call("v3:r-8", 41, "+13175550142", "+13175550151");
    other_sip["from"] = json!("sip:gencred43@sip.telnyx.com");
    let cases: [(&str, Value); 12] = [
        // Mark's line is not Rita's to call from.
        (
            "v3:r-1",
            app_call("v3:r-1", 41, "+13175550142", "+13175550154"),
        ),
        // A phone of another company.
        (
            "v3:r-2",
            app_call("v3:r-2", 49, "+13175550142", "+14635550201"),
        ),
        // Abroad, and a Caribbean +1 number.
        (
            "v3:r-3",
            app_call("v3:r-3", 41, "+442079460958", "+13175550151"),
        ),
        (
            "v3:r-4",
            app_call("v3:r-4", 41, "+18765550142", "+13175550151"),
        ),
        // Premium, pay-per-call and personal-communications numbers.
        (
            "v3:r-11",
            app_call("v3:r-11", 41, "+19005550142", "+13175550151"),
        ),
        (
            "v3:r-12",
            app_call("v3:r-12", 41, "+13179760142", "+13175550151"),
        ),
        (
            "v3:r-13",
            app_call("v3:r-13", 41, "+15005550142", "+13175550151"),
        ),
        // N11 codes that are no emergency: the app dials those itself.
        ("v3:r-14", app_call("v3:r-14", 41, "411", "+13175550151")),
        // A SIP user in `from` alone proves nothing (no call signature).
        (
            "v3:r-5",
            parked(
                "v3:r-5",
                "sip:gencred41@sip.telnyx.com",
                "+13175550142",
                &[("X-Granite-Device-Id", "960041")],
            ),
        ),
        ("v3:r-8", other_sip),
        // No phone named at all, or one that is not a phone.
        (
            "v3:r-6",
            parked("v3:r-6", "+13175550151", "+13175550142", &[]),
        ),
        (
            "v3:r-7",
            parked(
                "v3:r-7",
                "+13175550151",
                "+13175550142",
                &[("X-Granite-Device-Id", "not-a-number")],
            ),
        ),
    ];
    for (n, (ccid, payload)) in cases.iter().enumerate() {
        let body = event(&format!("evt-r{n}"), "call.initiated", payload);
        send(&server, Some(&signer), &body).await.assert_status_ok();
        let id = call_of(&pool, ccid).await;
        assert_eq!(
            stubs.telnyx(),
            [(
                "POST".to_string(),
                format!("/calls/{ccid}/actions/hangup"),
                json!({ "command_id": format!("hangup-{id}") })
            )],
            "{ccid}"
        );
        assert_eq!(
            state(&pool, id).await,
            ("failed".into(), false, true, None),
            "{ccid}"
        );
        stubs.clear_telnyx();
    }
    // Calling switched off for the company, or the company deleted.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100",
    )
    .await;
    send(
        &server,
        Some(&signer),
        &event(
            "evt-off",
            "call.initiated",
            &app_call("v3:r-15", 41, "+13175550142", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(stubs.telnyx_paths(), ["POST /calls/v3:r-15/actions/hangup"]);
    stubs.clear_telnyx();
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 1 WHERE company_id = 100",
    )
    .await;
    sql(
        &pool,
        "UPDATE company SET deleted_at = NOW() WHERE id = 100",
    )
    .await;
    send(
        &server,
        Some(&signer),
        &event(
            "evt-deleted",
            "call.initiated",
            &app_call("v3:r-16", 41, "+13175550142", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(stubs.telnyx_paths(), ["POST /calls/v3:r-16/actions/hangup"]);
    stubs.clear_telnyx();
    sql(&pool, "UPDATE company SET deleted_at = NULL WHERE id = 100").await;
    // A signed-out phone.
    revoke_session(&pool, 41).await;
    send(
        &server,
        Some(&signer),
        &event(
            "evt-revoked",
            "call.initiated",
            &app_call("v3:r-9", 41, "+13175550142", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(stubs.telnyx_paths(), ["POST /calls/v3:r-9/actions/hangup"]);
    // Only parked app calls are acted on.
    stubs.clear_telnyx();
    let mut bridging = app_call("v3:r-10", 41, "+13175550142", "+13175550151");
    bridging["state"] = json!("bridging");
    send(
        &server,
        Some(&signer),
        &event("evt-bridging", "call.initiated", &bridging),
    )
    .await
    .assert_status_ok();
    assert!(stubs.telnyx().is_empty());
}

#[test_db::test(seed)]
async fn the_daily_limit_and_six_a_minute_hold(pool: MySqlPool) {
    enable_calling(&pool, 100, "daily_call_limit = 2").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    let call = |n: u32| {
        event(
            &format!("evt-l{n}"),
            "call.initiated",
            &app_call(&format!("v3:l-{n}"), 41, "+13175550142", "+13175550151"),
        )
    };
    let actions = |stubs: &Stubs| {
        stubs
            .telnyx_paths()
            .into_iter()
            .map(|path| path.rsplit('/').next().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
    };
    for n in 1..=3 {
        send(&server, Some(&signer), &call(n))
            .await
            .assert_status_ok();
    }
    assert_eq!(actions(&stubs), ["transfer", "transfer", "hangup"]);
    stubs.clear_telnyx();
    // Room for the day again: two calls of the minute were used, four left.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET daily_call_limit = 100 WHERE company_id = 100",
    )
    .await;
    for n in 4..=8 {
        send(&server, Some(&signer), &call(n))
            .await
            .assert_status_ok();
    }
    assert_eq!(
        actions(&stubs),
        ["transfer", "transfer", "transfer", "transfer", "hangup"]
    );
}

#[test_db::test(seed)]
async fn emergency_codes_are_connected_and_alert_the_admins(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'both'").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 45, 102, 100).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    send(
        &server,
        Some(&signer),
        &event(
            "evt-911",
            "call.initiated",
            &app_call("v3:e-1", 41, "911", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    let id = call_of(&pool, "v3:e-1").await;
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/calls/v3:e-1/actions/transfer".to_string(),
            json!({ "to": "911", "from": "+13175550151", "timeout_secs": 60,
                    "webhook_url": format!("{}/outbound", webhook_base(100)),
                    "command_id": format!("approve-{id}") })
        )]
    );
    let pushes: Vec<(String, String)> = stubs
        .pushes()
        .iter()
        .map(|message| {
            (
                message["token"].as_str().unwrap().to_string(),
                message["data"]["type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(pushes, [("push-45".to_string(), "e911_alert".to_string())]);
    let alerts: Vec<(i32, String)> = sqlx::query_as(
        "SELECT user_id, message FROM notifications WHERE notification_type = 'e911_alert' ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        alerts,
        [
            (102, "Rita Rep dialed 911 on Granite Phone".to_string()),
            (103, "Rita Rep dialed 911 on Granite Phone".to_string())
        ]
    );
    let alerts_now = |pool: MySqlPool| async move {
        let count: i64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS SIGNED) FROM notifications WHERE notification_type = 'e911_alert'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        count
    };
    // The same phone again within 10 minutes: connected, no second alert.
    stubs.clear_telnyx();
    send(
        &server,
        Some(&signer),
        &event(
            "evt-911b",
            "call.initiated",
            &app_call("v3:e-3", 41, "911", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /calls/v3:e-3/actions/transfer"]
    );
    assert_eq!(alerts_now(pool.clone()).await, 2);
    // Never refused: calling off and no phone named, it still goes out,
    // from a company number; so does the 933 address test line.
    stubs.clear_telnyx();
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100",
    )
    .await;
    send(
        &server,
        Some(&signer),
        &event(
            "evt-988",
            "call.initiated",
            &parked("v3:e-2", "+13175550199", "sip:988@sip.telnyx.com", &[]),
        ),
    )
    .await
    .assert_status_ok();
    send(
        &server,
        Some(&signer),
        &event(
            "evt-933",
            "call.initiated",
            &parked("v3:e-4", "+13175550199", "933", &[]),
        ),
    )
    .await
    .assert_status_ok();
    let sent = stubs.telnyx();
    assert_eq!(sent[0].1, "/calls/v3:e-2/actions/transfer");
    assert_eq!(
        (&sent[0].2["to"], &sent[0].2["from"]),
        (&json!("988"), &json!("+13175550152"))
    );
    assert_eq!(
        (sent[1].1.as_str(), &sent[1].2["to"]),
        ("/calls/v3:e-4/actions/transfer", &json!("933"))
    );
    // The unproven phone's 988 alerted (naming no one); its 933 right after
    // is deduplicated with it (per company, for phones that prove nothing).
    assert_eq!(alerts_now(pool.clone()).await, 4);
}

#[test_db::test(seed)]
async fn a_hangup_before_the_answer_ends_the_call_unanswered(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    let hangup = |id: &str, ccid: &str, cause: &str, source: &str| {
        event(
            id,
            "call.hangup",
            &json!({ "call_control_id": ccid, "call_session_id": format!("session-{ccid}"),
                     "connection_id": "conn-100", "hangup_cause": cause, "hangup_source": source }),
        )
    };
    // Rita gives up while it rings.
    send(
        &server,
        Some(&signer),
        &event(
            "evt-h1",
            "call.initiated",
            &app_call("v3:h-1", 41, "+13175550142", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    let id = call_of(&pool, "v3:h-1").await;
    send(
        &server,
        Some(&signer),
        &hangup("evt-h2", "v3:h-1", "originator_cancel", "caller"),
    )
    .await
    .assert_status_ok();
    assert_eq!(
        state(&pool, id).await,
        ("canceled".into(), true, true, None)
    );
    // The customer's line is busy.
    send(
        &server,
        Some(&signer),
        &event(
            "evt-h3",
            "call.initiated",
            &app_call("v3:h-2", 41, "+13175550143", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    let busy = call_of(&pool, "v3:h-2").await;
    send(
        &server,
        Some(&signer),
        &hangup("evt-h4", "v3:h-2", "user_busy", "callee"),
    )
    .await
    .assert_status_ok();
    assert_eq!(state(&pool, busy).await, ("busy".into(), true, true, None));
    // A transfer Telnyx refuses: hung up, failed.
    stubs
        .telnyx_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    send(
        &server,
        Some(&signer),
        &event(
            "evt-h5",
            "call.initiated",
            &app_call("v3:h-3", 41, "+13175550144", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    let failed = call_of(&pool, "v3:h-3").await;
    assert_eq!(
        stubs.telnyx_paths()[2..],
        [
            "POST /calls/v3:h-3/actions/transfer".to_string(),
            "POST /calls/v3:h-3/actions/hangup".to_string()
        ]
    );
    assert_eq!(
        state(&pool, failed).await,
        ("failed".into(), false, true, None)
    );
}

/// Telnyx's `call.initiated` of parked call `payload` (event id from its
/// Call Control id).
async fn place(server: &axum_test::TestServer, signer: &Signer, payload: &Value) {
    let ccid = payload["call_control_id"].as_str().unwrap();
    let body = event(&format!("evt-{ccid}"), "call.initiated", payload);
    send(server, Some(signer), &body).await.assert_status_ok();
}

/// Call `ccid` was hung up (the only Telnyx request since the last clear)
/// and logged failed, nobody's and not approved.
async fn assert_refused(pool: &MySqlPool, stubs: &Stubs, ccid: &str) {
    let id = call_of(pool, ccid).await;
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            format!("/calls/{ccid}/actions/hangup"),
            json!({ "command_id": format!("hangup-{id}") })
        )],
        "{ccid}"
    );
    stubs.clear_telnyx();
    let row: (String, Option<i32>, bool) = sqlx::query_as(
        "SELECT CAST(status AS CHAR), user_id, approved <> 0 FROM phone_calls WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(row, ("failed".to_string(), None, false), "{ccid}");
}

/// A parked call from `caller` to `to` naming device `device`, signed with
/// `auth` when there is one.
fn named(ccid: &str, device: &str, to: &str, caller: &str, auth: Option<&str>) -> Value {
    let mut headers = vec![
        ("X-Granite-Device-Id", device),
        ("X-Granite-Caller-Id", caller),
    ];
    if let Some(auth) = auth {
        headers.push(("X-Granite-Call-Auth", auth));
    }
    parked(ccid, caller, to, &headers)
}

#[test_db::test(seed)]
async fn app_calls_must_be_signed_by_the_phone_that_places_them(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 43, 101, 100).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    // Signed by the phone that places it: approved.
    place(
        &server,
        &signer,
        &app_call("v3:s-1", 41, "+13175550142", "+13175550151"),
    )
    .await;
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /calls/v3:s-1/actions/transfer"]
    );
    assert_eq!(
        state(&pool, call_of(&pool, "v3:s-1").await).await.0,
        "ringing"
    );
    stubs.clear_telnyx();
    // Rita names Mark's phone to call from his line, signing with her own
    // call key: refused, and the call is nobody's.
    let as_mark = call_auth(41, "960043", "+13175550142", "+13175550154", now());
    place(
        &server,
        &signer,
        &named(
            "v3:s-2",
            "960043",
            "+13175550142",
            "+13175550154",
            Some(&as_mark),
        ),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-2").await;
    // Signed with her phone's old key (the hash of its device token, as
    // before call keys): refused.
    let old_key = {
        use base64::Engine as _;
        use hmac::{Hmac, KeyInit, Mac};
        use sha2::Digest as _;
        let key = sha2::Sha256::digest(phone_common::token(41).as_bytes());
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
        let time = now();
        mac.update(format!("v1|960041|+13175550142|+13175550151|{time}").as_bytes());
        format!(
            "v1.{time}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    };
    place(
        &server,
        &signer,
        &named(
            "v3:s-11",
            "960041",
            "+13175550142",
            "+13175550151",
            Some(&old_key),
        ),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-11").await;
    // Her own phone, but signed 10 minutes and a second ago (or ahead).
    for (ccid, time) in [("v3:s-3", now() - 601), ("v3:s-4", now() + 601)] {
        let stale = call_auth(41, "960041", "+13175550142", "+13175550151", time);
        place(
            &server,
            &signer,
            &named(ccid, "960041", "+13175550142", "+13175550151", Some(&stale)),
        )
        .await;
        assert_refused(&pool, &stubs, ccid).await;
    }
    // No signature, while `from` carries no SIP user.
    place(
        &server,
        &signer,
        &named("v3:s-5", "960041", "+13175550142", "+13175550151", None),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-5").await;
    // Signed for another number, or another caller ID, than the call has.
    let other_number = call_auth(41, "960041", "+13175550142", "+13175550151", now());
    place(
        &server,
        &signer,
        &named(
            "v3:s-6",
            "960041",
            "+13175550143",
            "+13175550151",
            Some(&other_number),
        ),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-6").await;
    let other_caller = call_auth(41, "960041", "+13175550143", "+13175550151", now());
    place(
        &server,
        &signer,
        &named(
            "v3:s-7",
            "960041",
            "+13175550143",
            "+13175550152",
            Some(&other_caller),
        ),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-7").await;
    // The SIP user in `from` alone proves nothing.
    place(
        &server,
        &signer,
        &parked(
            "v3:s-10",
            "sip:gencred43@sip.telnyx.com",
            "+13175550142",
            &[],
        ),
    )
    .await;
    assert_refused(&pool, &stubs, "v3:s-10").await;
    // Mark's phone, signed by it, with its own SIP user in `from`: his call.
    let by_mark = call_auth(43, "960043", "+13175550142", "", now());
    place(
        &server,
        &signer,
        &parked(
            "v3:s-8",
            "sip:gencred43@sip.telnyx.com",
            "+13175550142",
            &[
                ("X-Granite-Device-Id", "960043"),
                ("X-Granite-Call-Auth", &by_mark),
            ],
        ),
    )
    .await;
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /calls/v3:s-8/actions/transfer"]
    );
    let user: Option<i32> = sqlx::query_scalar("SELECT user_id FROM phone_calls WHERE id = ?")
        .bind(call_of(&pool, "v3:s-8").await)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user, Some(101));
    stubs.clear_telnyx();
    // 911 is never refused, but an unproven phone gets nobody's caller ID:
    // a company number calls, and the alert names no one.
    let forged = call_auth(41, "960043", "911", "+13175550154", now());
    place(
        &server,
        &signer,
        &named("v3:s-9", "960043", "911", "+13175550154", Some(&forged)),
    )
    .await;
    let sent = stubs.telnyx();
    assert_eq!(sent[0].1, "/calls/v3:s-9/actions/transfer");
    assert_eq!(
        (&sent[0].2["to"], &sent[0].2["from"]),
        (&json!("911"), &json!("+13175550152"))
    );
    let alert: String = sqlx::query_scalar(
        "SELECT message FROM notifications WHERE notification_type = 'e911_alert' ORDER BY id LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(alert, "A Granite Phone user dialed 911 on Granite Phone");
}

#[test_db::test(seed)]
async fn without_a_notice_recording_the_notice_is_said_before_recording(pool: MySqlPool) {
    // Outgoing calls are recorded, but the company saved no notice recording.
    enable_calling(&pool, 100, "recording_mode = 'both'").await;
    lines(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    let signer = Signer::new();
    let (server, stubs) = signed_server(&pool, &signer).await;
    let spoken_notice = "c3Bva2VuLW5vdGljZQ==";
    let leg = |ccid: &str, session: &str, client_state: Option<&str>| {
        let mut payload = json!({ "call_control_id": ccid, "call_session_id": session,
                                  "connection_id": "conn-100" });
        if let Some(state) = client_state {
            payload["client_state"] = json!(state);
        }
        payload
    };
    send(
        &server,
        Some(&signer),
        &event(
            "evt-n1",
            "call.initiated",
            &app_call("v3:app-n", 41, "+13175550142", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    let id = call_of(&pool, "v3:app-n").await;
    // A transfer can play only a file: bridged unrecorded, the customer's
    // leg marked for the spoken notice.
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/calls/v3:app-n/actions/transfer".to_string(),
            json!({ "to": "+13175550142", "from": "+13175550151", "timeout_secs": 60,
                    "webhook_url": format!("{}/outbound", webhook_base(100)),
                    "command_id": format!("approve-{id}"),
                    "target_leg_client_state": spoken_notice })
        )]
    );
    stubs.clear_telnyx();
    // The app's own leg answering is not the customer: nothing is said.
    send(
        &server,
        Some(&signer),
        &event(
            "evt-n2",
            "call.answered",
            &leg("v3:app-n", "session-v3:app-n", None),
        ),
    )
    .await
    .assert_status_ok();
    assert!(stubs.telnyx().is_empty());
    // The customer answers: the notice to both sides, then the recording.
    send(
        &server,
        Some(&signer),
        &event(
            "evt-n3",
            "call.answered",
            &leg("v3:cust-n", "session-v3:app-n", Some(spoken_notice)),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(
        stubs.telnyx(),
        [
            (
                "POST".to_string(),
                "/calls/v3:app-n/actions/speak".to_string(),
                json!({ "payload": "This call may be recorded.", "voice": "female",
                        "language": "en-US", "target_legs": "both",
                        "command_id": format!("notice-{id}") })
            ),
            (
                "POST".to_string(),
                "/calls/v3:app-n/actions/record_start".to_string(),
                json!({ "format": "mp3", "channels": "dual", "command_id": format!("record-{id}") })
            ),
        ]
    );
    assert_eq!(
        state(&pool, id).await,
        ("in_progress".into(), true, false, None)
    );
    // When the notice cannot be said, the call is not recorded.
    stubs.clear_telnyx();
    send(
        &server,
        Some(&signer),
        &event(
            "evt-m1",
            "call.initiated",
            &app_call("v3:app-m", 41, "+13175550143", "+13175550151"),
        ),
    )
    .await
    .assert_status_ok();
    stubs.clear_telnyx();
    stubs
        .telnyx_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    send(
        &server,
        Some(&signer),
        &event(
            "evt-m2",
            "call.answered",
            &leg("v3:cust-m", "session-v3:app-m", Some(spoken_notice)),
        ),
    )
    .await
    .assert_status_ok();
    assert_eq!(stubs.telnyx_paths(), ["POST /calls/v3:app-m/actions/speak"]);
    let answered: String = sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM phone_calls WHERE telnyx_call_control_id = 'v3:app-m'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(answered, "in_progress");
}
