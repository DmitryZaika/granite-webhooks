//! Granite Phone call settings: incoming call rules and "Forward all calls"
//! (`routes/phone/call_rules.rs`, `phone/call_rules.rs`), incoming call
//! information (display number, announcements, answer PIN), the company's
//! spam and blocked numbers (`routes/phone/blocking.rs`), and hold music
//! (`routes/phone/hold.rs`), with what they do to incoming calls
//! (`routes/telnyx_webhooks/inbound.rs`, `dial.rs`, `whisper.rs`).
//!
//! Fixture phones use n = 41..=60; numbers 9851+.

mod common;
mod phone_common;

use api::storage::MEMORY_PUBLIC_BASE;
use axum::http::{Method, StatusCode};
use chrono::{Datelike, Utc};
use phone_common::{
    HEADER, add_call, add_number, add_phone, add_session, assert_device_only, assign,
    enable_calling, phone_server, phone_server_with, post_texml, revoke_session, set_settings, sql,
    texml_doc, token, webhook_base,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita's direct line (rings only her), the sales line (rings Rita and
/// Mark), and company 101's line. Calling is on, nothing is recorded.
async fn setup(pool: &MySqlPool) {
    enable_calling(pool, 100, "recording_mode = 'off'").await;
    add_number(pool, 9851, 100, "+13175550151", "direct", "Rita direct").await;
    add_number(pool, 9852, 100, "+13175550152", "shared", "Sales").await;
    add_number(pool, 9853, 101, "+14635550153", "direct", "Olga direct").await;
    assign(pool, 9851, 100, true, true).await;
    assign(pool, 9852, 100, true, true).await;
    assign(pool, 9852, 101, true, true).await;
    assign(pool, 9853, 106, true, true).await;
}

/// Telnyx's voice-URL request for call `sid` from `from` to `to`, with
/// more fields.
async fn inbound(
    server: &axum_test::TestServer,
    sid: &str,
    from: &str,
    to: &str,
    extra: &[(&str, &str)],
) -> String {
    let mut form = vec![
        ("CallSid", sid),
        ("ConnectionId", "app-100"),
        ("From", from),
        ("To", to),
        ("CallStatus", "ringing"),
    ];
    form.extend_from_slice(extra);
    let res = post_texml(server, 100, "inbound", &form).await;
    res.assert_status_ok();
    res.text()
}

async fn call_row(pool: &MySqlPool, sid: &str) -> Option<(i64, String, Option<i32>, bool)> {
    sqlx::query_as(
        "SELECT CAST(id AS SIGNED), CAST(status AS CHAR), user_id, spam_likely <> 0
           FROM phone_calls WHERE telnyx_call_sid = ?",
    )
    .bind(sid)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// The `<Sip>` of phone `n` on call `id`, with an announcement for `user`.
fn sip(id: i64, n: u32, announce_user: Option<i32>) -> String {
    let base = webhook_base(100);
    let url = announce_user
        .map(|user| format!(" url=\"{base}/whisper?call={id}&amp;user={user}\""))
        .unwrap_or_default();
    format!(
        "<Sip{url} statusCallback=\"{base}/leg-status?call={id}&amp;device={}\" \
         statusCallbackEvent=\"initiated ringing answered completed\">sip:gencred{n}@sip.telnyx.com</Sip>",
        960_000 + n
    )
}

fn ring(id: i64, caller_id: Option<&str>, sips: &str) -> String {
    let base = webhook_base(100);
    let caller = caller_id
        .map(|number| format!(" callerId=\"{number}\""))
        .unwrap_or_default();
    format!(
        "<Dial answerOnBridge=\"true\" timeout=\"25\" action=\"{base}/dial-result?call={id}\"{caller}>{sips}</Dial>"
    )
}

/// Working hours on a weekday that is not today (UTC): outside them now.
fn hours_not_today() -> Value {
    let day = (Utc::now().weekday().num_days_from_sunday() + 3) % 7;
    json!({ "tz": "UTC", "days": [{ "day": day, "start": "00:00", "end": "24:00" }] })
}

#[test_db::test(seed)]
async fn the_new_routes_take_only_the_device_token(pool: MySqlPool) {
    add_session(&pool, 41, 100, 100).await;
    revoke_session(&pool, 41).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        41,
        &[
            (Method::GET, "/v1/phone/call-rules".into()),
            (Method::POST, "/v1/phone/call-rules".into()),
            (Method::PUT, "/v1/phone/call-rules/1".into()),
            (Method::DELETE, "/v1/phone/call-rules/1".into()),
            (Method::GET, "/v1/phone/lines".into()),
            (Method::GET, "/v1/phone/call-blocking".into()),
            (Method::PUT, "/v1/phone/call-blocking".into()),
            (Method::GET, "/v1/phone/hold-music".into()),
            (Method::POST, "/v1/phone/hold-music/upload".into()),
        ],
    )
    .await;
}

#[test_db::test(seed)]
async fn forward_all_call_information_and_the_pin_are_settings(pool: MySqlPool) {
    setup(&pool).await;
    add_session(&pool, 41, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let put = |body: Value| {
        server
            .put("/v1/phone/settings")
            .add_header(HEADER, token(41))
            .json(&body)
    };
    let res = put(json!({})).await;
    let settings: Value = res.json();
    assert_eq!(
        (
            &settings["forwardAll"],
            &settings["forwardAllTo"],
            &settings["displayNumber"],
            &settings["announceLineName"],
            &settings["requirePin"],
            &settings["holdMusic"],
            &settings["holdMusicUrl"],
        ),
        (
            &json!(false),
            &json!("voicemail"),
            &json!("caller"),
            &json!(true),
            &json!(false),
            &json!("calm"),
            &Value::Null
        )
    );
    // Forwarding to a number needs one that can be called.
    let res = put(json!({ "forwardAll": true, "forwardAllTo": "number" })).await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "invalid_phone");
    let res = put(json!({ "forwardAll": true, "forwardAllTo": "number",
                          "forwardAllNumber": "+44 20 7946 0958" }))
    .await;
    assert_eq!(res.json::<Value>()["error"], "invalid_phone");
    let res = put(json!({ "forwardAll": true, "forwardAllTo": "number",
                          "forwardAllNumber": "(317) 555-0188", "displayNumber": "called",
                          "unknownShowMine": true, "announceDirect": true,
                          "announceLineName": false }))
    .await;
    res.assert_status_ok();
    let settings: Value = res.json();
    assert_eq!(settings["forwardAllNumber"], "+13175550188");
    assert_eq!(settings["displayNumber"], "called");
    assert_eq!(settings["announceDirect"], true);
    assert_eq!(settings["announceShared"], false);
    // The PIN: 4 digits, needed before it can be required, never sent back.
    let res = put(json!({ "requirePin": true })).await;
    assert_eq!(res.json::<Value>()["error"], "pin_required");
    let res = put(json!({ "answerPin": "12a4" })).await;
    assert_eq!(res.json::<Value>()["error"], "invalid_pin");
    let res = put(json!({ "answerPin": "1234", "requirePin": true })).await;
    res.assert_status_ok();
    let text = res.text();
    assert!(text.contains("\"requirePin\":true"), "{text}");
    assert!(!text.contains("1234"), "{text}");
    let hash: String =
        sqlx::query_scalar("SELECT answer_pin_hash FROM phone_user_settings WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hash, api::phone::settings::pin_hash(100, "1234"));
    // Off and on again keeps the PIN; clearing it turns it off.
    assert_eq!(
        put(json!({ "requirePin": false })).await.json::<Value>()["requirePin"],
        false
    );
    assert_eq!(
        put(json!({ "requirePin": true })).await.json::<Value>()["requirePin"],
        true
    );
    assert_eq!(
        put(json!({ "answerPin": null })).await.json::<Value>()["requirePin"],
        false
    );
}

#[test_db::test(seed)]
async fn rules_are_mine_checked_and_ordered(pool: MySqlPool) {
    setup(&pool).await;
    add_session(&pool, 41, 100, 100).await;
    add_session(&pool, 42, 101, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let create = |n: u32, body: Value| {
        server
            .post("/v1/phone/call-rules")
            .add_header(HEADER, token(n))
            .json(&body)
    };
    let res = create(
        41,
        json!({ "name": " VIPs ", "callers": [{ "number": "317-555-0100" }, { "customerId": 1004 }],
                "numberIds": [9851], "action": "forward", "forwardTo": "3175550177" }),
    )
    .await;
    res.assert_status(StatusCode::CREATED);
    let vip: Value = res.json();
    let id = vip["id"].as_i64().unwrap();
    assert_eq!(
        vip,
        json!({ "id": id, "name": "VIPs", "enabled": true,
                "callers": [{ "number": "+13175550100" }, { "customerId": 1004 }],
                "schedule": null, "numberIds": [9851], "action": "forward",
                "forwardTo": "+13175550177", "message": null })
    );
    // What is wrong is refused.
    for (body, error) in [
        (json!({ "name": "", "action": "ring" }), None),
        (
            json!({ "name": "x", "action": "forward" }),
            Some("invalid_phone"),
        ),
        (
            json!({ "name": "x", "action": "message", "message": " " }),
            None,
        ),
        (
            json!({ "name": "x", "action": "ring", "callers": [{ "customerId": 2000 }] }),
            Some("unknown_contact"),
        ),
        (
            json!({ "name": "x", "action": "ring", "callers": [{}] }),
            None,
        ),
        (
            json!({ "name": "x", "action": "ring", "numberIds": [9853] }),
            Some("not_my_number"),
        ),
        (
            json!({ "name": "x", "action": "ring", "schedule": { "tz": "UTC", "days": [] } }),
            None,
        ),
    ] {
        let res = create(41, body.clone()).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        if let Some(error) = error {
            assert_eq!(res.json::<Value>()["error"], error, "{body}");
        }
    }
    let res = create(
        41,
        json!({ "name": "Evenings", "schedule": hours_not_today(), "action": "message",
                "message": "Call back tomorrow.", "enabled": false }),
    )
    .await;
    res.assert_status(StatusCode::CREATED);
    let evenings = res.json::<Value>()["id"].as_i64().unwrap();
    let list = |n: u32| {
        server
            .get("/v1/phone/call-rules")
            .add_header(HEADER, token(n))
    };
    let names: Vec<Value> = list(41).await.json::<Value>()["rules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| rule["name"].clone())
        .collect();
    assert_eq!(names, [json!("VIPs"), json!("Evenings")]);
    // Mark sees none of them and cannot change or delete them.
    assert_eq!(list(42).await.json::<Value>(), json!({ "rules": [] }));
    server
        .put(&format!("/v1/phone/call-rules/{id}"))
        .add_header(HEADER, token(42))
        .json(&json!({ "name": "Mine", "action": "ring" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/phone/call-rules/{id}"))
        .add_header(HEADER, token(42))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Rita turns Evenings on (the whole rule is sent), then deletes VIPs.
    let res = server
        .put(&format!("/v1/phone/call-rules/{evenings}"))
        .add_header(HEADER, token(41))
        .json(
            &json!({ "name": "Evenings", "schedule": hours_not_today(), "action": "message",
                       "message": "Call back tomorrow.", "enabled": true }),
        )
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>()["enabled"], true);
    server
        .delete(&format!("/v1/phone/call-rules/{id}"))
        .add_header(HEADER, token(41))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        list(41).await.json::<Value>()["rules"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // At most 20 rules.
    for n in 1..20 {
        create(
            41,
            json!({ "name": format!("Rule {n}"), "action": "voicemail" }),
        )
        .await
        .assert_status(StatusCode::CREATED);
    }
    let res = create(41, json!({ "name": "One too many", "action": "voicemail" })).await;
    assert_eq!(res.json::<Value>()["error"], "too_many_rules");
}

#[test_db::test(seed)]
async fn my_lines_say_what_unanswered_calls_do(pool: MySqlPool) {
    setup(&pool).await;
    sql(
        &pool,
        "UPDATE telnyx_numbers SET no_answer_action = 'voicemail' WHERE id = 9852",
    )
    .await;
    add_session(&pool, 41, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let res = server
        .get("/v1/phone/lines")
        .add_header(HEADER, token(41))
        .await;
    // The direct line's AI Assistant: none set, so voicemail.
    assert_eq!(
        res.json::<Value>(),
        json!({ "lines": [
            { "numberId": 9851, "e164": "+13175550151", "label": "Rita direct", "kind": "direct",
              "rings": true, "mineAlone": true, "noAnswer": "voicemail" },
            { "numberId": 9852, "e164": "+13175550152", "label": "Sales", "kind": "shared",
              "rings": true, "mineAlone": false, "noAnswer": "voicemail" },
        ] })
    );
    sql(
        &pool,
        "UPDATE telnyx_company_config SET ai_assistant_id = 'assistant-100' WHERE company_id = 100",
    )
    .await;
    let res = server
        .get("/v1/phone/lines")
        .add_header(HEADER, token(41))
        .await;
    assert_eq!(res.json::<Value>()["lines"][0]["noAnswer"], "ai");
}

#[test_db::test(seed)]
async fn forward_all_takes_my_own_line_but_not_the_shared_one(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 42, 101, 100).await;
    set_settings(
        &pool,
        100,
        100,
        "forward_all = 1, forward_all_to = 'number', forward_all_number = '+13175550188'",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // Her direct line: forwarded whole, hers, and no missed-call push.
    let text = inbound(&server, "v3:fw-1", "+13175550100", "+13175550151", &[]).await;
    assert_eq!(text, texml_doc("<Dial>+13175550188</Dial>"));
    let (_, status, user, _) = call_row(&pool, "v3:fw-1").await.unwrap();
    assert_eq!((status.as_str(), user), ("missed", Some(100)));
    assert!(stubs.pushes().is_empty());
    // The sales line still rings Mark; Rita's forward takes nothing from him.
    let text = inbound(&server, "v3:fw-2", "+13175550100", "+13175550152", &[]).await;
    let (id, status, user, _) = call_row(&pool, "v3:fw-2").await.unwrap();
    assert_eq!(text, texml_doc(&ring(id, None, &sip(id, 42, None))));
    assert_eq!((status.as_str(), user), ("ringing", None));
    // To voicemail: the direct line's greeting, then the recording.
    sql(
        &pool,
        "UPDATE phone_user_settings SET forward_all_to = 'voicemail' WHERE user_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:fw-3", "+13175550100", "+13175550151", &[]).await;
    assert!(
        text.contains("<Say>Sorry, nobody can take your call right now."),
        "{text}"
    );
    assert!(text.contains("<Record "), "{text}");
    // Do not disturb wins over it: the number's no-answer action as before.
    sql(
        &pool,
        "UPDATE phone_user_settings SET dnd_indefinite = 1 WHERE user_id = 100",
    )
    .await;
    inbound(&server, "v3:fw-4", "+13175550100", "+13175550151", &[]).await;
    let (_, status, user, _) = call_row(&pool, "v3:fw-4").await.unwrap();
    assert_eq!((status.as_str(), user), ("missed", None));
}

#[test_db::test(seed)]
async fn rules_match_callers_and_ring_outside_hours(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 42, 101, 100).await;
    set_settings(
        &pool,
        100,
        100,
        &format!("working_hours = '{}'", hours_not_today()),
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    let create = |body: Value| {
        server
            .post("/v1/phone/call-rules")
            .add_header(HEADER, token(41))
            .json(&body)
    };
    // Erin (customer 1004, 317-555-0105) rings Rita even outside her hours.
    create(json!({ "name": "Erin", "callers": [{ "customerId": 1004 }], "action": "ring" }))
        .await
        .assert_status(StatusCode::CREATED);
    // A message for one number on her direct line.
    create(
        json!({ "name": "Robot", "callers": [{ "number": "+13175550166" }],
                   "numberIds": [9851], "action": "message", "message": "Please stop calling." }),
    )
    .await
    .assert_status(StatusCode::CREATED);
    let text = inbound(&server, "v3:r-1", "+13175550105", "+13175550151", &[]).await;
    let (id, ..) = call_row(&pool, "v3:r-1").await.unwrap();
    assert_eq!(text, texml_doc(&ring(id, None, &sip(id, 41, None))));
    // Anyone else, outside her hours: the number's no-answer action.
    let text = inbound(&server, "v3:r-2", "+13175550100", "+13175550151", &[]).await;
    assert!(text.contains("<Record "), "{text}");
    let text = inbound(&server, "v3:r-3", "+13175550166", "+13175550151", &[]).await;
    assert_eq!(text, texml_doc("<Say>Please stop calling.</Say><Hangup/>"));
    // On the sales line the message rule is not for that number, and Erin
    // rings Rita and Mark both.
    let text = inbound(&server, "v3:r-4", "+13175550105", "+13175550152", &[]).await;
    let (id, ..) = call_row(&pool, "v3:r-4").await.unwrap();
    assert_eq!(
        text,
        texml_doc(&ring(
            id,
            None,
            &[sip(id, 41, None), sip(id, 42, None)].concat()
        ))
    );
}

#[test_db::test(seed)]
async fn announcements_and_the_number_shown(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    add_phone(&pool, 42, 101, 100).await;
    set_settings(
        &pool,
        100,
        100,
        "announce_shared = 1, display_number = 'called'",
    )
    .await;
    set_settings(&pool, 101, 100, "unknown_show_mine = 1").await;
    let (server, _) = phone_server(pool.clone()).await;
    // Mark wants the caller's number: the caller's number it is.
    let text = inbound(&server, "v3:a-1", "+13175550100", "+13175550152", &[]).await;
    let (id, ..) = call_row(&pool, "v3:a-1").await.unwrap();
    assert_eq!(
        text,
        texml_doc(&ring(
            id,
            None,
            &[sip(id, 41, Some(100)), sip(id, 42, None)].concat()
        ))
    );
    // A hidden number: both want the line's number then.
    let text = inbound(&server, "v3:a-2", "anonymous", "+13175550152", &[]).await;
    let (id, ..) = call_row(&pool, "v3:a-2").await.unwrap();
    assert_eq!(
        text,
        texml_doc(&ring(
            id,
            Some("+13175550152"),
            &[sip(id, 41, Some(100)), sip(id, 42, None)].concat()
        ))
    );
    // Rita hears who calls which line once her phone answers.
    let res = post_texml(
        &server,
        100,
        &format!("whisper?call={id}&user=100"),
        &[("CallSid", "v3:a-2-leg")],
    )
    .await;
    let base = webhook_base(100);
    assert_eq!(
        res.text(),
        texml_doc(&format!(
            "<Gather numDigits=\"1\" timeout=\"1\" action=\"{base}/whisper?call={id}&amp;user=100&amp;done=1\">\
             <Say>Call for Sales from an unknown number.</Say></Gather>"
        ))
    );
    // Without the line's name; and once Mark answered, her phone hangs up.
    sql(
        &pool,
        "UPDATE phone_user_settings SET announce_line_name = 0 WHERE user_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:a-3", "+13175550100", "+13175550152", &[]).await;
    let (id, ..) = call_row(&pool, "v3:a-3").await.unwrap();
    assert!(text.contains("whisper?call="), "{text}");
    let res = post_texml(
        &server,
        100,
        &format!("whisper?call={id}&user=100"),
        &[("CallSid", "v3:a-3-leg")],
    )
    .await;
    assert!(
        res.text().contains("<Say>Call from Alice Lead.</Say>"),
        "{}",
        res.text()
    );
    sql(
        &pool,
        &format!(
            "UPDATE phone_calls SET answered_at = UTC_TIMESTAMP(), user_id = 101 WHERE id = {id}"
        ),
    )
    .await;
    let res = post_texml(
        &server,
        100,
        &format!("whisper?call={id}&user=100"),
        &[("CallSid", "v3:a-3-leg2")],
    )
    .await;
    assert_eq!(res.text(), texml_doc("<Hangup/>"));
    // A key press connects at once.
    let res = post_texml(
        &server,
        100,
        &format!("whisper?call={id}&user=100&done=1"),
        &[("CallSid", "v3:a-3-leg"), ("Digits", "5")],
    )
    .await;
    assert_eq!(res.text(), texml_doc(""));
}

#[test_db::test(seed)]
async fn a_fallback_cell_asks_for_the_pin(pool: MySqlPool) {
    setup(&pool).await;
    set_settings(
        &pool,
        100,
        100,
        &format!(
            "cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(), cell_fallback = 1, \
             require_pin = 1, answer_pin_hash = '{}'",
            api::phone::settings::pin_hash(100, "4321")
        ),
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    // No phone: her cell rings first.
    let text = inbound(&server, "v3:p-1", "+13175550100", "+13175550151", &[]).await;
    let (id, ..) = call_row(&pool, "v3:p-1").await.unwrap();
    assert!(text.contains("cell-screen?call="), "{text}");
    let base = webhook_base(100);
    let screen = post_texml(
        &server,
        100,
        &format!("cell-screen?call={id}&user=100"),
        &[("CallSid", "v3:p-1-cell"), ("To", "+13175550177")],
    )
    .await;
    assert_eq!(
        screen.text(),
        texml_doc(&format!(
            "<Gather numDigits=\"4\" timeout=\"8\" action=\"{base}/cell-screen-result?call={id}&amp;user=100\">\
             <Say>Granite Phone: a call from Alice Lead. Enter your PIN to answer.</Say></Gather><Hangup/>"
        ))
    );
    let route = format!("cell-screen-result?call={id}&user=100");
    let answer = async |sid: &str, digits: &str| {
        post_texml(
            &server,
            100,
            &route,
            &[("CallSid", sid), ("To", "+13175550177"), ("Digits", digits)],
        )
        .await
        .text()
    };
    // "1" is not the PIN, nor is a wrong one.
    assert_eq!(answer("v3:p-1-a", "1").await, texml_doc("<Hangup/>"));
    assert_eq!(answer("v3:p-1-b", "1234").await, texml_doc("<Hangup/>"));
    assert_eq!(answer("v3:p-1-c", "4321").await, texml_doc(""));
    let user: Option<i32> = sqlx::query_scalar("SELECT user_id FROM phone_calls WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user, Some(100));
}

#[test_db::test(seed)]
async fn only_admins_see_and_save_the_companys_blocking(pool: MySqlPool) {
    setup(&pool).await;
    add_session(&pool, 41, 100, 100).await;
    add_session(&pool, 42, 102, 100).await;
    add_session(&pool, 43, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    let get = |n: u32| {
        server
            .get("/v1/phone/call-blocking")
            .add_header(HEADER, token(n))
    };
    let res = get(41).await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["error"], "admin_only");
    let defaults = json!({
        "spamAction": "ring", "blockedAction": "announce",
        "announcement": "I'm sorry. The number you dialed can not be reached from your calling area. Goodbye.",
        "defaultAnnouncement": true, "blockNoCallerId": false, "blocked": [], "trusted": [],
    });
    assert_eq!(get(42).await.json::<Value>(), defaults);
    let save = |n: u32, body: Value| {
        server
            .put("/v1/phone/call-blocking")
            .add_header(HEADER, token(n))
            .json(&body)
    };
    save(
        41,
        json!({ "spamAction": "block", "blockedAction": "hangup" }),
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
    let res = save(
        42,
        json!({ "spamAction": "voicemail", "blockedAction": "hangup", "blockNoCallerId": true,
                "announcement": " We don't take calls from you. ",
                "blocked": ["(317) 555-0666", "212", "212", "+1 317 555 0666"],
                "trusted": ["212-555-0100"] }),
    )
    .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "spamAction": "voicemail", "blockedAction": "hangup",
                "announcement": "We don't take calls from you.", "defaultAnnouncement": false,
                "blockNoCallerId": true, "blocked": ["+13175550666", "212"],
                "trusted": ["+12125550100"] })
    );
    // Another company's admin sees its own defaults.
    sql(&pool, "UPDATE users SET is_admin = 1 WHERE id = 106").await;
    assert_eq!(get(43).await.json::<Value>(), defaults);
    // An area code is never trusted; nonsense is refused.
    let res = save(
        42,
        json!({ "spamAction": "ring", "blockedAction": "announce", "trusted": ["317"] }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "invalid_entry");
    let res = save(
        42,
        json!({ "spamAction": "ring", "blockedAction": "announce", "blocked": ["hello"] }),
    )
    .await;
    assert_eq!(res.json::<Value>()["error"], "invalid_entry");
    // Audited, with counts only.
    let details: String = sqlx::query_scalar(
        "SELECT CAST(details AS CHAR) FROM phone_audit_log WHERE action = 'call_blocking.save'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!details.contains("555"), "{details}");
}

#[test_db::test(seed)]
async fn blocked_calls_never_ring(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    sql(
        &pool,
        "INSERT INTO phone_call_blocking (company_id, blocked_action) VALUES (100, 'announce')",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO phone_call_block_list (company_id, list, pattern) VALUES
           (100, 'blocked', '+13175550666'), (100, 'blocked', '212'),
           (100, 'trusted', '+12125550100')",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // A blocked number hears the announcement; nothing is in the call log.
    let text = inbound(&server, "v3:b-1", "+13175550666", "+13175550151", &[]).await;
    assert_eq!(
        text,
        texml_doc(
            "<Say>I&apos;m sorry. The number you dialed can not be reached from your calling area. \
             Goodbye.</Say><Hangup/>"
        )
    );
    assert!(call_row(&pool, "v3:b-1").await.is_none());
    // A blocked area code, hung up on; a trusted number in it rings.
    sql(
        &pool,
        "UPDATE phone_call_blocking SET blocked_action = 'hangup' WHERE company_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:b-2", "+12125550199", "+13175550151", &[]).await;
    assert_eq!(text, texml_doc("<Reject/>"));
    let text = inbound(&server, "v3:b-3", "+12125550100", "+13175550151", &[]).await;
    let (id, ..) = call_row(&pool, "v3:b-3").await.unwrap();
    assert_eq!(text, texml_doc(&ring(id, None, &sip(id, 41, None))));
    // No caller ID, once the company blocks those.
    inbound(&server, "v3:b-4", "anonymous", "+13175550151", &[]).await;
    assert!(call_row(&pool, "v3:b-4").await.is_some());
    sql(
        &pool,
        "UPDATE phone_call_blocking SET block_no_caller_id = 1 WHERE company_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:b-5", "Restricted", "+13175550151", &[]).await;
    assert_eq!(text, texml_doc("<Reject/>"));
    // Voicemail: logged as a call (without a missed-call push), the
    // voicemail greeting, nothing rings.
    sql(
        &pool,
        "UPDATE phone_call_blocking SET blocked_action = 'voicemail' WHERE company_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:b-6", "+13175550666", "+13175550151", &[]).await;
    assert!(text.contains("<Record "), "{text}");
    assert!(!text.contains("<Dial"), "{text}");
    let (_, status, ..) = call_row(&pool, "v3:b-6").await.unwrap();
    assert_eq!(status, "missed");
    // The blocked ones are logged apart, with why (a repeat once).
    inbound(&server, "v3:b-1", "+13175550666", "+13175550151", &[]).await;
    let logged: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT telnyx_call_sid, reason, action FROM phone_blocked_calls ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        logged,
        [
            ("v3:b-1".into(), "number".into(), "announce".into()),
            ("v3:b-2".into(), "area_code".into(), "hangup".into()),
            ("v3:b-5".into(), "no_caller_id".into(), "hangup".into()),
            ("v3:b-6".into(), "number".into(), "voicemail".into()),
        ]
    );
    assert!(stubs.pushes().is_empty());
}

#[test_db::test(seed)]
async fn likely_spam_is_marked_and_handled_as_the_company_says(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let failed = [
        ("ShakenStirAttestation", "C"),
        ("ShakenStirValidated", "false"),
    ];
    // Allowed to ring (the default), marked as spam.
    let text = inbound(&server, "v3:s-1", "+13175550123", "+13175550151", &failed).await;
    let (id, _, _, spam) = call_row(&pool, "v3:s-1").await.unwrap();
    assert_eq!(text, texml_doc(&ring(id, None, &sip(id, 41, None))));
    assert!(spam);
    // Attestation C alone, or A, is not spam.
    inbound(
        &server,
        "v3:s-2",
        "+13175550123",
        "+13175550151",
        &[("ShakenStirAttestation", "C")],
    )
    .await;
    assert!(!call_row(&pool, "v3:s-2").await.unwrap().3);
    // Telnyx's call screening flag is.
    inbound(
        &server,
        "v3:s-3",
        "+13175550123",
        "+13175550151",
        &[("CallerId", "SPAM LIKELY")],
    )
    .await;
    assert!(call_row(&pool, "v3:s-3").await.unwrap().3);
    // Sent to voicemail: no ring, no push.
    sql(
        &pool,
        "INSERT INTO phone_call_blocking (company_id, spam_action) VALUES (100, 'voicemail')",
    )
    .await;
    let text = inbound(&server, "v3:s-4", "+13175550123", "+13175550151", &failed).await;
    assert!(text.contains("<Record "), "{text}");
    let (_, status, _, spam) = call_row(&pool, "v3:s-4").await.unwrap();
    assert_eq!((status.as_str(), spam), ("missed", true));
    // Blocked: the action for blocked calls.
    sql(
        &pool,
        "UPDATE phone_call_blocking SET spam_action = 'block' WHERE company_id = 100",
    )
    .await;
    let text = inbound(&server, "v3:s-5", "+13175550123", "+13175550151", &failed).await;
    assert!(
        text.contains("<Hangup/>") && text.contains("<Say>I&apos;m sorry"),
        "{text}"
    );
    // A trusted number is never spam-blocked.
    sql(&pool, "INSERT INTO phone_call_block_list (company_id, list, pattern) VALUES (100, 'trusted', '+13175550123')").await;
    let text = inbound(&server, "v3:s-6", "+13175550123", "+13175550151", &failed).await;
    assert!(text.contains("<Dial"), "{text}");
    assert!(stubs.pushes().is_empty());
}

#[test_db::test(seed)]
async fn hold_music_tracks_and_my_upload(pool: MySqlPool) {
    setup(&pool).await;
    add_session(&pool, 41, 100, 100).await;
    add_session(&pool, 42, 101, 100).await;
    let mut storage = None;
    let (server, _) = phone_server_with(pool.clone(), |state| {
        storage = Some(state.storage.clone());
        state
    })
    .await;
    let storage = storage.unwrap();
    let res = server
        .get("/v1/phone/hold-music")
        .add_header(HEADER, token(41))
        .await;
    assert_eq!(
        res.json::<Value>(),
        json!({ "tracks": [
            { "id": "calm", "name": "Calm piano", "url": "https://api.test/v1/public/hold-music/calm.mp3" },
            { "id": "bright", "name": "Bright marimba", "url": "https://api.test/v1/public/hold-music/bright.mp3" },
            { "id": "ambient", "name": "Soft ambient", "url": "https://api.test/v1/public/hold-music/ambient.mp3" },
        ], "maxUploadBytes": 5_242_880 })
    );
    // The tracks are public MP3s.
    let mp3 = server.get("/v1/public/hold-music/bright.mp3").await;
    mp3.assert_status_ok();
    assert_eq!(mp3.header("content-type"), "audio/mpeg");
    assert!(mp3.as_bytes().len() > 10_000);
    server
        .get("/v1/public/hold-music/other.mp3")
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let put = |n: u32, body: Value| {
        server
            .put("/v1/phone/settings")
            .add_header(HEADER, token(n))
            .json(&body)
    };
    assert_eq!(
        put(41, json!({ "holdMusic": "ambient" }))
            .await
            .json::<Value>()["holdMusic"],
        "ambient"
    );
    assert_eq!(
        put(41, json!({ "holdMusic": "jazz" }))
            .await
            .json::<Value>()["error"],
        "invalid_hold_music"
    );
    // Custom without an upload yet.
    assert_eq!(
        put(41, json!({ "holdMusic": "custom" }))
            .await
            .json::<Value>()["error"],
        "invalid_hold_music"
    );
    let upload = |n: u32, name: &str, kind: &str| {
        server
            .post("/v1/phone/hold-music/upload")
            .add_header(HEADER, token(n))
            .json(&json!({ "fileName": name, "contentType": kind }))
    };
    let res = upload(41, "tune.ogg", "audio/ogg").await;
    assert_eq!(res.json::<Value>()["error"], "invalid_hold_music");
    let res = upload(41, "Jingle.MP3", "").await;
    res.assert_status(StatusCode::CREATED);
    let started: Value = res.json();
    assert_eq!(started["contentType"], "audio/mpeg");
    let url = started["url"].as_str().unwrap().to_string();
    let key = url
        .strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap()
        .to_string();
    assert!(
        key.starts_with("hold-music/100/100-") && key.ends_with(".mp3"),
        "{key}"
    );
    // Not put yet; then not audio (and deleted); then a real MP3.
    let choose =
        json!({ "holdMusic": "custom", "holdMusicUrl": url, "holdMusicName": "Jingle.MP3" });
    assert_eq!(
        put(41, choose.clone()).await.json::<Value>()["error"],
        "invalid_hold_music"
    );
    storage
        .put(&key, b"<html>".to_vec(), "audio/mpeg")
        .await
        .unwrap();
    assert_eq!(
        put(41, choose.clone()).await.json::<Value>()["error"],
        "invalid_hold_music"
    );
    assert!(!storage.memory_keys().contains(&key));
    storage
        .put(&key, b"ID3\x04\x00\x00music".to_vec(), "audio/mpeg")
        .await
        .unwrap();
    // Mark cannot take Rita's upload.
    assert_eq!(
        put(42, choose.clone()).await.json::<Value>()["error"],
        "invalid_hold_music"
    );
    let settings: Value = put(41, choose).await.json();
    assert_eq!(
        (
            &settings["holdMusic"],
            &settings["holdMusicUrl"],
            &settings["holdMusicName"]
        ),
        (&json!("custom"), &json!(url), &json!("Jingle.MP3"))
    );
    // Back to a track keeps the upload; a new upload replaces (deletes) it.
    put(41, json!({ "holdMusic": "calm" }))
        .await
        .assert_status_ok();
    assert_eq!(
        put(41, json!({ "holdMusic": "custom" }))
            .await
            .json::<Value>()["holdMusicUrl"],
        url
    );
    let second = upload(41, "b.wav", "audio/wav").await.json::<Value>();
    let second_url = second["url"].as_str().unwrap();
    let second_key = second_url
        .strip_prefix(&format!("{MEMORY_PUBLIC_BASE}/"))
        .unwrap();
    storage
        .put(
            second_key,
            b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec(),
            "audio/wav",
        )
        .await
        .unwrap();
    put(
        41,
        json!({ "holdMusic": "custom", "holdMusicUrl": second_url }),
    )
    .await
    .assert_status_ok();
    assert!(!storage.memory_keys().contains(&key));
    assert!(storage.memory_keys().contains(&second_key.to_string()));
}

#[test_db::test(seed)]
async fn holding_a_call_plays_my_music_to_the_other_party(pool: MySqlPool) {
    setup(&pool).await;
    add_phone(&pool, 41, 100, 100).await;
    // An app call (Call Control) and an incoming call (TeXML), both live.
    add_call(
        &pool,
        980_001,
        100,
        Some(100),
        "outbound",
        "ringing",
        "+13175550100",
        "2026-10-24 10:00:00",
        "telnyx_call_control_id = 'v3:out-leg', telnyx_call_session_id = 'sess-1', \
              answered_at = UTC_TIMESTAMP(), started_at = UTC_TIMESTAMP()",
    )
    .await;
    add_call(
        &pool,
        980_002,
        100,
        Some(100),
        "inbound",
        "ringing",
        "+13175550101",
        "2026-10-24 10:00:00",
        "telnyx_call_sid = 'v3:in-call', number_id = 9851, our_number = '+13175550151', \
              answered_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE, started_at = UTC_TIMESTAMP()",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let hold = |held: bool, ids: Value| {
        server
            .put("/v1/phone/calls/hold")
            .add_header(HEADER, token(41))
            .json(&json!({ "held": held, "telnyxCallIds": ids }))
    };
    let res = hold(true, json!(["sess-1"])).await;
    assert_eq!(
        res.json::<Value>(),
        json!({ "callId": 980_001, "onHold": true, "music": true })
    );
    let res = hold(true, json!(["sess-1"])).await;
    assert_eq!(res.json::<Value>()["onHold"], true);
    // Swapped: the incoming call is resumed... it was never held: nothing.
    hold(false, json!(["v3:in-call"])).await.assert_status_ok();
    let res = hold(false, json!(["sess-1"])).await;
    assert_eq!(
        res.json::<Value>(),
        json!({ "callId": 980_001, "onHold": false, "music": false })
    );
    let res = hold(true, json!(["v3:in-call"])).await;
    assert_eq!(res.json::<Value>()["music"], true);
    let seen: Vec<(String, Value)> = stubs
        .telnyx()
        .into_iter()
        .map(|(method, path, body)| (format!("{method} {path}"), body))
        .collect();
    let calm = "https://api.test/v1/public/hold-music/calm.mp3";
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0].0, "POST /calls/v3:out-leg/actions/playback_start");
    assert_eq!(
        (
            &seen[0].1["audio_url"],
            &seen[0].1["loop"],
            &seen[0].1["target_legs"]
        ),
        (&json!(calm), &json!("infinity"), &json!("opposite"))
    );
    assert_eq!(seen[1].0, "POST /calls/v3:out-leg/actions/playback_stop");
    assert_eq!(seen[1].1["stop"], "all");
    assert_eq!(seen[2].0, "POST /calls/v3:in-call/actions/playback_start");
    assert_eq!(seen[2].1["target_legs"], "self");
    // The end of the call clears the hold.
    let on_hold: bool =
        sqlx::query_scalar("SELECT on_hold <> 0 FROM phone_calls WHERE id = 980002")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(on_hold);
    // No music chosen: on hold, silence, Telnyx untouched.
    set_settings(&pool, 100, 100, "hold_music = 'none'").await;
    stubs.clear_telnyx();
    let res = hold(true, json!(["sess-1"])).await;
    assert_eq!(
        res.json::<Value>(),
        json!({ "callId": 980_001, "onHold": true, "music": false })
    );
    assert!(stubs.telnyx().is_empty());
    // Telnyx refusing the music still leaves the call on hold.
    sql(
        &pool,
        "UPDATE phone_user_settings SET hold_music = 'bright' WHERE user_id = 100",
    )
    .await;
    hold(false, json!(["sess-1"])).await.assert_status_ok();
    stubs
        .telnyx_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let res = hold(true, json!(["sess-1"])).await;
    assert_eq!(
        res.json::<Value>(),
        json!({ "callId": 980_001, "onHold": true, "music": false })
    );
    stubs
        .telnyx_down
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // Call via my cell cannot be held here; no live call is 404.
    sql(
        &pool,
        "UPDATE phone_calls SET via_cell = 1 WHERE id = 980001",
    )
    .await;
    hold(true, json!(["sess-1"]))
        .await
        .assert_status(StatusCode::CONFLICT);
    sql(&pool, "UPDATE phone_calls SET ended_at = UTC_TIMESTAMP()").await;
    hold(true, json!([]))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
