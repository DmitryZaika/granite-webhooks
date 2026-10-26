//! Telnyx's inbound TeXML (`routes/telnyx_webhooks/inbound.rs`): who an
//! incoming call rings, the TeXML answers, the call log, and the token,
//! number, account and signature checks.
//!
//! Fixture phones use n = 1..=20; numbers 9801+.

use crate::phone_common;

use axum::body::Bytes;
use axum::http::StatusCode;
use chrono::{Datelike, Utc};
use phone_common::{
    Signer, add_number, add_phone, assign, enable_calling, hook_path, phone_server,
    phone_server_with, post_texml, revoke_session, set_settings, signed_config, sql, texml_doc,
    webhook_base,
};
use serde_json::json;
use sqlx::MySqlPool;

/// Rita's direct line (rings her), the sales line (rings Rita and Mark),
/// the main line (rings Rita), and company 101's line (Olga's).
async fn lines(pool: &MySqlPool) {
    add_number(pool, 9801, 100, "+13175550151", "direct", "Rita direct").await;
    add_number(pool, 9802, 100, "+13175550152", "shared", "Sales").await;
    add_number(pool, 9803, 100, "+13175550150", "main", "Main line").await;
    add_number(pool, 9804, 101, "+14635550201", "direct", "Olga direct").await;
    assign(pool, 9801, 100, true, true).await;
    assign(pool, 9802, 100, true, true).await;
    assign(pool, 9802, 101, true, true).await;
    assign(pool, 9803, 100, true, false).await;
    assign(pool, 9804, 106, true, true).await;
}

/// Telnyx's voice-URL request for call `sid` from `from` to `to`.
async fn inbound(
    server: &axum_test::TestServer,
    company_id: i32,
    sid: &str,
    from: &str,
    to: &str,
) -> axum_test::TestResponse {
    let account = format!("acct-{company_id}");
    let app = format!("app-{company_id}");
    post_texml(
        server,
        company_id,
        "inbound",
        &[
            ("AccountSid", account.as_str()),
            ("CallSid", sid),
            ("ConnectionId", app.as_str()),
            ("From", from),
            ("To", to),
            ("CallStatus", "ringing"),
            ("Timestamp", "2026-10-18T10:00:00Z"),
        ],
    )
    .await
}

/// Our call id of Telnyx's call `sid`.
async fn call_id(pool: &MySqlPool, sid: &str) -> i64 {
    sqlx::query_scalar("SELECT CAST(id AS SIGNED) FROM phone_calls WHERE telnyx_call_sid = ?")
        .bind(sid)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn status(pool: &MySqlPool, id: i64) -> String {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM phone_calls WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The `<Sip>` of phone `device` (SIP user `sip`) on call `id`.
fn sip(base: &str, id: i64, device: i64, sip: &str) -> String {
    format!(
        "<Sip statusCallback=\"{base}/leg-status?call={id}&amp;device={device}\" \
         statusCallbackEvent=\"initiated ringing answered completed\">sip:{sip}@sip.telnyx.com</Sip>"
    )
}

/// The `<Dial>` ringing `sips` on call `id`, recorded.
fn ring(base: &str, id: i64, timeout: i64, sips: &str) -> String {
    format!(
        "<Dial answerOnBridge=\"true\" timeout=\"{timeout}\" action=\"{base}/dial-result?call={id}\" \
         record=\"record-from-answer-dual\" recordingStatusCallback=\"{base}/recording?call={id}&amp;kind=call\">\
         {sips}</Dial>"
    )
}

const NOTICE: &str = "<Say>This call may be recorded.</Say>";

/// The AI Assistant's TeXML for call `id`.
fn assistant(base: &str, id: i64) -> String {
    format!(
        "<Connect action=\"{base}/after-ai?call={id}\"><AIAssistant id=\"assistant-100\"/></Connect>"
    )
}

/// Working hours on a weekday that is not today (UTC): outside them now.
fn hours_not_today() -> String {
    let day = (Utc::now().weekday().num_days_from_sunday() + 3) % 7;
    format!(r#"{{"tz":"UTC","days":[{{"day":{day},"start":"00:00","end":"24:00"}}]}}"#)
}

#[test_db::test(seed)]
async fn the_sales_line_rings_every_phone_of_its_users(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    add_phone(&pool, 1, 100, 100).await;
    add_phone(&pool, 2, 100, 100).await;
    add_phone(&pool, 3, 101, 100).await;
    // A revoked phone and one without a Telnyx credential do not ring.
    add_phone(&pool, 4, 101, 100).await;
    revoke_session(&pool, 4).await;
    add_phone(&pool, 5, 100, 100).await;
    sql(
        &pool,
        "UPDATE phone_devices SET sip_username = NULL WHERE id = 960005",
    )
    .await;
    // Mark rings for 40 seconds: the longest ring timeout wins.
    set_settings(&pool, 101, 100, "ring_timeout_secs = 40").await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let res = inbound(&server, 100, "v3:in-1", "+13175550100", "+13175550152").await;
    res.assert_status_ok();
    assert_eq!(res.header("content-type"), "application/xml");
    let id = call_id(&pool, "v3:in-1").await;
    let base = webhook_base(100);
    let sips = [
        sip(&base, id, 960_001, "gencred1"),
        sip(&base, id, 960_002, "gencred2"),
        sip(&base, id, 960_003, "gencred3"),
    ]
    .concat();
    let expected = texml_doc(&format!("{NOTICE}{}", ring(&base, id, 40, &sips)));
    assert_eq!(res.text(), expected);
    // Logged ringing: Alice's call (her oldest open deal), three phones.
    let row: (
        String,
        String,
        String,
        Option<i32>,
        Option<i64>,
        Option<i32>,
        Option<String>,
        i32,
        Option<i32>,
    ) = sqlx::query_as(
        "SELECT CAST(direction AS CHAR), CAST(status AS CHAR), other_number, customer_id,
                    CAST(deal_id AS SIGNED), number_id, our_number, devices_rung, user_id
               FROM phone_calls WHERE id = ? AND company_id = 100",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "inbound".into(),
            "ringing".into(),
            "+13175550100".into(),
            Some(1000),
            Some(8007),
            Some(9802),
            Some("+13175550152".into()),
            3,
            None
        )
    );
    let legs: Vec<(i64, String)> = sqlx::query_as(
        "SELECT CAST(device_id AS SIGNED), status FROM phone_call_legs WHERE call_id = ? ORDER BY device_id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        legs,
        [
            (960_001, "queued".to_string()),
            (960_002, "queued".to_string()),
            (960_003, "queued".to_string())
        ]
    );
    // Telnyx asks again: the same answer, one call, one stored event.
    let again = inbound(&server, 100, "v3:in-1", "+13175550100", "+13175550152").await;
    assert_eq!(again.text(), expected);
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM phone_calls WHERE telnyx_call_sid = 'v3:in-1'),
                (SELECT COUNT(*) FROM phone_call_legs WHERE call_id = ?),
                (SELECT COUNT(*) FROM phone_call_events WHERE call_id = ? AND event_type = 'inbound'
                    AND source = 'texml' AND company_id = 100)",
    )
    .bind(id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 3, 1));
    // Phones ring through Telnyx's own push: ours stay quiet.
    assert!(stubs.pushes().is_empty());
}

#[test_db::test(seed)]
async fn users_on_do_not_disturb_or_off_hours_are_not_rung(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    add_phone(&pool, 1, 100, 100).await;
    add_phone(&pool, 3, 101, 100).await;
    set_settings(&pool, 101, 100, "dnd_indefinite = 1").await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let res = inbound(&server, 100, "v3:in-1", "+13175550199", "+13175550152").await;
    let id = call_id(&pool, "v3:in-1").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "{NOTICE}{}",
            ring(&base, id, 25, &sip(&base, id, 960_001, "gencred1"))
        ))
    );
    // Rita outside her working hours as well: nobody rings, the AI
    // Assistant takes the call, logged missed (nobody to push to).
    set_settings(
        &pool,
        100,
        100,
        &format!("working_hours = '{}'", hours_not_today()),
    )
    .await;
    let res = inbound(&server, 100, "v3:in-2", "+13175550199", "+13175550152").await;
    let id = call_id(&pool, "v3:in-2").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!("{NOTICE}{}", assistant(&base, id)))
    );
    assert_eq!(status(&pool, id).await, "missed");
    assert!(stubs.pushes().is_empty());
    // Mark's Do not disturb that ended rings him again.
    sql(&pool, "UPDATE phone_user_settings SET dnd_indefinite = 0, dnd_until = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE user_id = 101").await;
    let res = inbound(&server, 100, "v3:in-3", "+13175550199", "+13175550152").await;
    let id = call_id(&pool, "v3:in-3").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "{NOTICE}{}",
            ring(&base, id, 25, &sip(&base, id, 960_003, "gencred3"))
        ))
    );
}

#[test_db::test(seed)]
async fn the_main_line_rings_only_while_the_company_is_open(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    add_phone(&pool, 1, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    // No business hours: always open.
    let res = inbound(&server, 100, "v3:main-1", "+13175550199", "+13175550150").await;
    let id = call_id(&pool, "v3:main-1").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "{NOTICE}{}",
            ring(&base, id, 25, &sip(&base, id, 960_001, "gencred1"))
        ))
    );
    // Closed today: the no-answer action at once.
    sql(
        &pool,
        &format!(
            "UPDATE telnyx_company_config SET business_hours = '{}' WHERE company_id = 100",
            hours_not_today()
        ),
    )
    .await;
    let res = inbound(&server, 100, "v3:main-2", "+13175550199", "+13175550150").await;
    let id = call_id(&pool, "v3:main-2").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!("{NOTICE}{}", assistant(&base, id)))
    );
    assert_eq!(status(&pool, id).await, "missed");
    // Open all week, but today is a holiday.
    let today = Utc::now().format("%Y-%m-%d").to_string();
    sql(
        &pool,
        &format!(
            r#"UPDATE telnyx_company_config
                  SET business_hours = '{{"tz":"UTC","days":[{}]}}', holidays = '["{today}"]'
                WHERE company_id = 100"#,
            (0..7)
                .map(|day| format!(r#"{{"day":{day},"start":"00:00","end":"24:00"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
    .await;
    let res = inbound(&server, 100, "v3:main-3", "+13175550199", "+13175550150").await;
    let id = call_id(&pool, "v3:main-3").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!("{NOTICE}{}", assistant(&base, id)))
    );
    // Rita's direct line keeps ringing her.
    let res = inbound(&server, 100, "v3:direct-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:direct-1").await;
    assert!(res.text().contains(&sip(&base, id, 960_001, "gencred1")));
}

#[test_db::test(seed)]
async fn with_no_phone_to_ring_the_call_goes_on_at_once(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    // Rita's phone has no Telnyx credential: it cannot ring, but gets our push.
    add_phone(&pool, 1, 100, 100).await;
    sql(&pool, "UPDATE phone_devices SET sip_username = NULL, telnyx_credential_id = NULL WHERE id = 960001").await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let res = inbound(&server, 100, "v3:in-1", "+13175550100", "+13175550151").await;
    let id = call_id(&pool, "v3:in-1").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!("{NOTICE}{}", assistant(&base, id)))
    );
    assert_eq!(status(&pool, id).await, "missed");
    assert_eq!(
        stubs.pushes(),
        [json!({ "token": "push-1",
                 "data": { "granite": "1", "type": "missed_call", "callId": id.to_string() },
                 "android": { "priority": "HIGH", "ttl": "60s" },
                 "apns": { "headers": { "apns-push-type": "alert", "apns-priority": "10" },
                           "payload": { "aps": { "alert": { "title": "Missed call" },
                                                 "sound": "default", "mutable-content": 1 } } } })]
    );
    // With cell fallback on, her verified cell rings first, behind "press 1".
    set_settings(
        &pool,
        100,
        100,
        "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), cell_fallback = 1",
    )
    .await;
    let res = inbound(&server, 100, "v3:in-2", "+13175550100", "+13175550151").await;
    let id = call_id(&pool, "v3:in-2").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "{NOTICE}<Dial timeout=\"25\" action=\"{base}/dial-result?call={id}&amp;cell=1\" \
             record=\"record-from-answer-dual\" recordingStatusCallback=\"{base}/recording?call={id}&amp;kind=call\">\
             <Number url=\"{base}/cell-screen?call={id}&amp;user=100\">+13175550177</Number></Dial>"
        ))
    );
    assert_eq!(status(&pool, id).await, "ringing");
    assert_eq!(stubs.pushes().len(), 1);
}

#[test_db::test(seed)]
async fn a_slow_push_never_holds_the_callers_answer(pool: MySqlPool) {
    enable_calling(&pool, 100, "ai_assistant_id = 'assistant-100'").await;
    lines(&pool).await;
    // Rita's two phones can only get our pushes, and FCM is slow.
    for n in [2, 3] {
        add_phone(&pool, n, 100, 100).await;
        sql(&pool, &format!("UPDATE phone_devices SET sip_username = NULL, telnyx_credential_id = NULL WHERE id = {}", 960_000 + n)).await;
    }
    let (server, stubs) = phone_server(pool.clone()).await;
    stubs
        .slow_pushes
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let base = webhook_base(100);
    let started = std::time::Instant::now();
    let res = inbound(&server, 100, "v3:slow-1", "+13175550100", "+13175550151").await;
    let took = started.elapsed();
    let id = call_id(&pool, "v3:slow-1").await;
    // The TeXML comes once the pushes' 2 seconds are up, not after FCM's 5
    // (each).
    assert_eq!(
        res.text(),
        texml_doc(&format!("{NOTICE}{}", assistant(&base, id)))
    );
    assert!(took < std::time::Duration::from_millis(4_500), "{took:?}");
    assert_eq!(status(&pool, id).await, "missed");
}

#[test_db::test(seed)]
async fn without_an_assistant_the_caller_leaves_a_voicemail(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'off'").await;
    lines(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let voicemail = |id: i64, greeting: &str| {
        texml_doc(&format!(
            "<Say>{greeting}</Say><Record maxLength=\"120\" playBeep=\"true\" \
             action=\"{base}/voicemail-done?call={id}\" \
             recordingStatusCallback=\"{base}/recording?call={id}&amp;kind=voicemail\"/>"
        ))
    };
    // The number wants the AI Assistant, the company has none.
    let res = inbound(&server, 100, "v3:vm-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:vm-1").await;
    assert_eq!(
        res.text(),
        voicemail(
            id,
            "Sorry, nobody can take your call right now. Please leave a message after the tone."
        )
    );
    // The number's own greeting (escaped).
    sql(&pool, "UPDATE telnyx_numbers SET no_answer_action = 'voicemail', greeting = 'You reached Rita & Co.' WHERE id = 9801").await;
    let res = inbound(&server, 100, "v3:vm-2", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:vm-2").await;
    assert_eq!(res.text(), voicemail(id, "You reached Rita &amp; Co."));
    // A forward.
    sql(&pool, "UPDATE telnyx_numbers SET no_answer_action = 'forward', forward_to = '317-555-0188' WHERE id = 9801").await;
    let res = inbound(&server, 100, "v3:fw-1", "+13175550199", "+13175550151").await;
    assert_eq!(res.text(), texml_doc("<Dial>+13175550188</Dial>"));
}

#[test_db::test(seed)]
async fn recording_off_neither_announces_nor_records(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'off'").await;
    lines(&pool).await;
    add_phone(&pool, 1, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let base = webhook_base(100);
    let res = inbound(&server, 100, "v3:in-1", "+13175550199", "+13175550151").await;
    let id = call_id(&pool, "v3:in-1").await;
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "<Dial answerOnBridge=\"true\" timeout=\"25\" action=\"{base}/dial-result?call={id}\">{}</Dial>",
            sip(&base, id, 960_001, "gencred1")
        ))
    );
    // Calling switched off: nothing rings.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 0 WHERE company_id = 100",
    )
    .await;
    let res = inbound(&server, 100, "v3:in-2", "+13175550199", "+13175550151").await;
    assert!(!res.text().contains("<Sip"));
    assert!(res.text().contains("<Record "));
}

#[test_db::test(seed)]
async fn calls_must_be_to_the_tokens_company(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    enable_calling(&pool, 101, "").await;
    lines(&pool).await;
    add_phone(&pool, 6, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    let form = |account: &'static str, connection: &'static str, to: &'static str| {
        [
            ("AccountSid", account),
            ("CallSid", "v3:x"),
            ("ConnectionId", connection),
            ("From", "+13175550199"),
            ("To", to),
        ]
    };
    server
        .post("/v1/webhooks/telnyx/tok00000000000000000000000000999/inbound")
        .form(&form("acct-100", "app-100", "+13175550151"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for (account, connection, to) in [
        // Company 101's number with company 100's token.
        ("acct-100", "app-100", "+14635550201"),
        // A number no company has.
        ("acct-100", "app-100", "+13175550999"),
        // Another Telnyx account or connection.
        ("acct-101", "app-100", "+13175550151"),
        ("acct-100", "app-101", "+13175550151"),
    ] {
        server
            .post(&hook_path(100, "inbound"))
            .form(&form(account, connection, to))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    // No account named, while the company saved its account id.
    let unnamed = [
        ("CallSid", "v3:unnamed"),
        ("From", "+13175550199"),
        ("To", "+13175550151"),
    ];
    server
        .post(&hook_path(100, "inbound"))
        .form(&unnamed)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM phone_calls")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(logged, 0);
    // A company that saved none is not checked for it.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET telnyx_account_id = NULL WHERE company_id = 100",
    )
    .await;
    server
        .post(&hook_path(100, "inbound"))
        .form(&unnamed)
        .await
        .assert_status_ok();
    // The credential connection's id is the company's too; company 101's
    // number with its own token rings Olga.
    let res = server
        .post(&hook_path(101, "inbound"))
        .form(&form("acct-101", "conn-101", "+14635550201"))
        .await;
    res.assert_status_ok();
    assert!(res.text().contains("sip:gencred6@sip.telnyx.com"));
}

#[test_db::test(seed)]
async fn signed_requests_must_check_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    lines(&pool).await;
    let signer = Signer::new();
    let key = signer.public_key.clone();
    let (server, _) = phone_server_with(pool.clone(), move |state| {
        state.with_phone_config(signed_config(&key))
    })
    .await;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("AccountSid", "acct-100")
        .append_pair("CallSid", "v3:signed-1")
        .append_pair("From", "+13175550199")
        .append_pair("To", "+13175550151")
        .finish();
    let post = |headers: &[(String, String)]| {
        let mut request = server
            .post(&hook_path(100, "inbound"))
            .content_type("application/x-www-form-urlencoded")
            .bytes(Bytes::from(body.clone()));
        for (name, value) in headers {
            request = request.add_header(name.as_str(), value.as_str());
        }
        request
    };
    post(&signer.headers(body.as_bytes()))
        .await
        .assert_status_ok();
    // Signed by another key, for another body, or only half the headers.
    let other = Signer::new();
    post(&other.headers(body.as_bytes()))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    post(&signer.headers(b"CallSid=v3:other"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let [signature, _] = signer.headers(body.as_bytes());
    post(&[signature])
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // Telnyx documents no signature for TeXML: without the headers it is
    // checked by the token and the account.
    post(&[]).await.assert_status_ok();
}
