//! Granite Phone's admin pages (`routes/phone/admin.rs`,
//! `routes/phone/superuser.rs`): the company's numbers (list, change,
//! import from Telnyx), its signed-in phones (list, sign out), and a
//! company's Telnyx setup (superusers). Changes land in `phone_audit_log`.
//!
//! Fixture phones use n = 111..=130; numbers 9901+.

mod common;
mod phone_common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use phone_common::{
    HEADER, PUBLIC_API, add_number, add_phone, add_session, assign, enable_calling, phone_server,
    revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::Ordering;

/// The main line, the sales line (Rita rings, Mark may call from it), Rita's
/// direct line, and a number of company 101.
async fn numbers(pool: &MySqlPool) {
    add_number(pool, 9901, 100, "+13175550160", "main", "Main line").await;
    add_number(pool, 9902, 100, "+13175550161", "shared", "Sales").await;
    add_number(pool, 9903, 100, "+13175550162", "direct", "Rita direct").await;
    add_number(pool, 9904, 101, "+14635550260", "direct", "Olga direct").await;
    assign(pool, 9902, 100, true, false).await;
    assign(pool, 9902, 101, false, true).await;
    assign(pool, 9903, 100, true, true).await;
    assign(pool, 9904, 106, true, true).await;
    // An assignment to another company's user never shows (or counts).
    assign(pool, 9902, 106, true, true).await;
}

/// The audit log of the company: (actor, action, target, details).
async fn audit_log(
    pool: &MySqlPool,
    company_id: i32,
) -> Vec<(Option<i32>, String, Option<String>, Value)> {
    let rows: Vec<(Option<i32>, String, Option<String>, String)> = sqlx::query_as(
        "SELECT actor_user_id, action, target, CAST(details AS CHAR) FROM phone_audit_log
          WHERE company_id = ? ORDER BY id",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(actor, action, target, details)| {
            (
                actor,
                action,
                target,
                serde_json::from_str(&details).unwrap(),
            )
        })
        .collect()
}

#[test_db::test(seed)]
async fn admins_list_and_change_the_company_numbers(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'both'").await;
    numbers(&pool).await;
    let (server, _) = phone_server(pool.clone()).await;
    let listed = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::ADMIN)
        .await;
    listed.assert_status_ok();
    let number = |id: i32, e164: &str, label: &str, kind: &str, assignments: Value| {
        json!({ "id": id, "e164": e164, "label": label, "kind": kind, "noAnswerAction": "ai",
                "forwardTo": null, "greeting": null, "active": true, "assignments": assignments })
    };
    assert_eq!(
        listed.json::<Value>(),
        json!({
            "numbers": [
                number(9901, "+13175550160", "Main line", "main", json!([])),
                number(9902, "+13175550161", "Sales", "shared", json!([
                    { "userId": 100, "rings": true, "canCallFrom": false },
                    { "userId": 101, "rings": false, "canCallFrom": true }])),
                number(9903, "+13175550162", "Rita direct", "direct", json!([
                    { "userId": 100, "rings": true, "canCallFrom": true }])),
            ],
            "users": [
                { "id": 102, "name": "Ada Admin" }, { "id": 107, "name": "Dina Dealer" },
                { "id": 101, "name": "Mark Manager" }, { "id": 100, "name": "Rita Rep" }],
            "config": { "enabled": true, "recordingMode": "both", "configured": true }
        })
    );
    // Not admins: refused; no session: 401.
    for session in [sessions::REP, sessions::VIEWER] {
        server
            .get("/v1/phone/admin/numbers")
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .get("/v1/phone/admin/numbers")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // The app's device token is not a way in.
    add_phone(&pool, 111, 102, 100).await;
    server
        .get("/v1/phone/admin/numbers")
        .add_header(HEADER, token(111))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    // A super admin of company 101 sees that company's numbers; no setup there.
    let other = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json::<Value>();
    assert_eq!(other["numbers"].as_array().unwrap().len(), 1);
    assert_eq!(other["numbers"][0]["id"], 9904);
    assert_eq!(
        other["users"],
        json!([{ "id": 106, "name": "Olga Other" }, { "id": 103, "name": "Sam Super" }])
    );
    assert_eq!(
        other["config"],
        json!({ "enabled": false, "recordingMode": "inbound", "configured": false })
    );

    // The sales line forwards now, and rings Rita and Mark; Ada has neither
    // flag and is left out.
    let body = json!({
        "label": " Sales team ", "kind": "shared", "noAnswerAction": "forward",
        "forwardTo": "(317) 555-0188", "greeting": "  ", "active": true,
        "assignments": [
            { "userId": 100, "rings": true, "canCallFrom": true },
            { "userId": 101, "rings": true, "canCallFrom": false },
            { "userId": 102, "rings": false, "canCallFrom": false }] });
    let saved = server
        .put("/v1/phone/admin/numbers/9902")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await;
    saved.assert_status_ok();
    let expected = json!({
        "id": 9902, "e164": "+13175550161", "label": "Sales team", "kind": "shared",
        "noAnswerAction": "forward", "forwardTo": "+13175550188", "greeting": null, "active": true,
        "assignments": [
            { "userId": 100, "rings": true, "canCallFrom": true },
            { "userId": 101, "rings": true, "canCallFrom": false }] });
    assert_eq!(saved.json::<Value>(), expected);
    // Olga's old row is gone with the rest.
    let rows: Vec<i32> = sqlx::query_scalar(
        "SELECT user_id FROM telnyx_number_users WHERE number_id = 9902 ORDER BY user_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, [100, 101]);
    let log = audit_log(&pool, 100).await;
    assert_eq!(log.len(), 1);
    let (actor, action, target, details) = &log[0];
    assert_eq!(
        (actor, action.as_str(), target.as_deref()),
        (&Some(102), "number_updated", Some("number:9902"))
    );
    assert_eq!(details["before"]["label"], "Sales");
    assert_eq!(
        details["before"]["assignments"].as_array().unwrap().len(),
        2
    );
    assert_eq!(details["after"]["forwardTo"], "+13175550188");
    assert_eq!(details["after"]["assignments"], expected["assignments"]);

    // Refused changes, and nothing saved.
    let with = |changes: Value| {
        let mut changed = body.clone();
        for (key, value) in changes.as_object().unwrap() {
            changed[key] = value.clone();
        }
        changed
    };
    for (bad, status) in [
        // Olga is in company 101; nobody is user 999999.
        (
            with(json!({ "assignments": [{ "userId": 106, "rings": true, "canCallFrom": true }] })),
            StatusCode::BAD_REQUEST,
        ),
        (
            with(
                json!({ "assignments": [{ "userId": 999_999, "rings": true, "canCallFrom": true }] }),
            ),
            StatusCode::BAD_REQUEST,
        ),
        (
            with(json!({ "assignments": [
            { "userId": 100, "rings": true, "canCallFrom": true },
            { "userId": 100, "rings": false, "canCallFrom": true }] })),
            StatusCode::BAD_REQUEST,
        ),
        (with(json!({ "forwardTo": null })), StatusCode::BAD_REQUEST),
        (
            with(json!({ "forwardTo": "+442079460958" })),
            StatusCode::BAD_REQUEST,
        ),
        (
            with(json!({ "label": "x".repeat(101) })),
            StatusCode::BAD_REQUEST,
        ),
        (
            with(json!({ "greeting": "x".repeat(501) })),
            StatusCode::BAD_REQUEST,
        ),
        (with(json!({ "kind": "personal" })), StatusCode::BAD_REQUEST),
    ] {
        server
            .put("/v1/phone/admin/numbers/9902")
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(status);
    }
    // Another company's number, and a rep.
    server
        .put("/v1/phone/admin/numbers/9904")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/phone/admin/numbers/9902")
        .with_session(sessions::REP)
        .json(&body)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(audit_log(&pool, 100).await.len(), 1);
    // Voicemail with a greeting, inactive, nobody assigned.
    let quiet = server
        .put("/v1/phone/admin/numbers/9902")
        .with_session(sessions::ADMIN)
        .json(&json!({ "label": "Sales", "kind": "shared", "noAnswerAction": "voicemail",
                       "forwardTo": null, "greeting": " Leave a message for Acme. ", "active": false,
                       "assignments": [] }))
        .await
        .json::<Value>();
    assert_eq!(
        (
            &quiet["greeting"],
            &quiet["active"],
            &quiet["assignments"],
            &quiet["forwardTo"]
        ),
        (
            &json!("Leave a message for Acme."),
            &json!(false),
            &json!([]),
            &json!(null)
        )
    );
}

#[test_db::test(seed)]
async fn numbers_are_imported_from_the_texml_app(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    numbers(&pool).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // The main line is here already; one number is company 101's; two are new.
    stubs.add_telnyx_number("num-1", "+13175550160");
    stubs.add_telnyx_number("num-2", "+13175550163");
    stubs.add_telnyx_number("num-3", "+14635550260");
    stubs.add_telnyx_number("num-4", "+13175550164");
    let imported = server
        .post("/v1/phone/admin/numbers/import")
        .with_session(sessions::ADMIN)
        .json(&json!({}))
        .await;
    imported.assert_status_ok();
    let body = imported.json::<Value>();
    assert_eq!(body["imported"], 2);
    let numbers: Vec<(&str, &str)> = body["numbers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|number| {
            (
                number["e164"].as_str().unwrap(),
                number["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        numbers,
        [
            ("+13175550160", "main"),
            ("+13175550161", "shared"),
            // New ones: direct lines without a label (sorted first), then by id.
            ("+13175550163", "direct"),
            ("+13175550164", "direct"),
            ("+13175550162", "direct"),
        ]
    );
    let new = &body["numbers"][2];
    assert_eq!(
        (
            &new["label"],
            &new["noAnswerAction"],
            &new["active"],
            &new["assignments"]
        ),
        (&json!(""), &json!("ai"), &json!(true), &json!([]))
    );
    let telnyx_ids: Vec<(String, Option<String>, i32)> = sqlx::query_as(
        "SELECT e164, telnyx_number_id, company_id FROM telnyx_numbers
          WHERE e164 IN ('+13175550160', '+13175550163', '+13175550164', '+14635550260') ORDER BY e164",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        telnyx_ids,
        [
            ("+13175550160".into(), Some("num-1".into()), 100),
            ("+13175550163".into(), Some("num-2".into()), 100),
            ("+13175550164".into(), Some("num-4".into()), 100),
            ("+14635550260".into(), None, 101),
        ]
    );
    // Read from the company's TeXML app.
    let paths = stubs.telnyx_paths();
    assert_eq!(paths.len(), 1);
    assert!(
        paths[0].starts_with("GET /phone_numbers?filter%5Bconnection_id%5D=app-100&"),
        "{}",
        paths[0]
    );
    let log = audit_log(&pool, 100).await;
    assert_eq!(
        log,
        [(
            Some(102),
            "numbers_imported".into(),
            Some("texml_app:app-100".into()),
            json!({ "imported": 2 })
        )]
    );
    // Again: nothing new, nothing logged.
    let again = server
        .post("/v1/phone/admin/numbers/import")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>();
    assert_eq!(again["imported"], 0);
    assert_eq!(audit_log(&pool, 100).await.len(), 1);
    // Telnyx failing is a 502; a company without a TeXML app a 400.
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    server
        .post("/v1/phone/admin/numbers/import")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_GATEWAY);
    server
        .post("/v1/phone/admin/numbers/import")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/phone/admin/numbers/import")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn admins_see_and_sign_out_the_company_phones(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_phone(&pool, 111, 100, 100).await;
    add_phone(&pool, 112, 101, 100).await;
    // Signed in, never registered for calling.
    add_session(&pool, 113, 107, 100).await;
    // Signed out, and another company's.
    add_phone(&pool, 114, 100, 100).await;
    revoke_session(&pool, 114).await;
    add_phone(&pool, 115, 106, 101).await;
    sql(
        &pool,
        "UPDATE device_sessions SET created_at = created_at - INTERVAL 1 HOUR WHERE id = 950111",
    )
    .await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let listed = server
        .get("/v1/phone/admin/devices")
        .with_session(sessions::ADMIN)
        .await;
    listed.assert_status_ok();
    let devices = listed.json::<Value>()["devices"].clone();
    let rows: Vec<Value> = devices
        .as_array()
        .unwrap()
        .iter()
        .map(|device| {
            json!([
                device["id"],
                device["userId"],
                device["userName"],
                device["sipUsername"],
                device["deviceName"],
                device["platform"],
                device["appVersion"],
                device["current"]
            ])
        })
        .collect();
    assert_eq!(
        rows,
        [
            json!([
                950_113,
                107,
                "Dina Dealer",
                null,
                "Test phone",
                "android",
                "1.0.0",
                false
            ]),
            json!([
                950_112,
                101,
                "Mark Manager",
                "gencred112",
                "Test phone",
                "android",
                "1.0.0",
                false
            ]),
            json!([
                950_111,
                100,
                "Rita Rep",
                "gencred111",
                "Test phone",
                "android",
                "1.0.0",
                false
            ]),
        ]
    );
    assert!(devices[0]["createdAt"].as_str().unwrap().ends_with('Z'));
    assert!(devices[0]["lastSeenAt"].as_str().is_some());
    server
        .get("/v1/phone/admin/devices")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Ada signs Mark's phone out: its token stops working, the phone wipes
    // itself, its credential goes.
    server
        .delete("/v1/phone/admin/devices/950112")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(112))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let reasons: (Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT ds.revoked_reason, pd.revoke_reason, pd.credential_deleted_at IS NOT NULL
           FROM device_sessions ds JOIN phone_devices pd ON pd.device_session_id = ds.id
          WHERE ds.id = 950112",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        reasons,
        (
            Some("admin_revoked".into()),
            Some("admin_revoked".into()),
            true
        )
    );
    assert_eq!(
        stubs.telnyx_paths(),
        ["DELETE /telephony_credentials/cred-112"]
    );
    let pushes = stubs.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        (&pushes[0]["token"], &pushes[0]["data"]["type"]),
        (&json!("push-112"), &json!("revoked"))
    );
    assert_eq!(
        audit_log(&pool, 100).await,
        [(
            Some(102),
            "device_revoked".into(),
            Some("device_session:950112".into()),
            json!({ "userId": 101 })
        )]
    );
    // Rita's phone still works.
    server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(111))
        .await
        .assert_status_ok();
    // Signed out already, another company's, unknown, or not an admin.
    for (id, session, status) in [
        (950_112, sessions::ADMIN, StatusCode::NOT_FOUND),
        (950_114, sessions::ADMIN, StatusCode::NOT_FOUND),
        (950_115, sessions::ADMIN, StatusCode::NOT_FOUND),
        (999_999, sessions::ADMIN, StatusCode::NOT_FOUND),
        (950_111, sessions::REP, StatusCode::FORBIDDEN),
    ] {
        server
            .delete(&format!("/v1/phone/admin/devices/{id}"))
            .with_session(session)
            .await
            .assert_status(status);
    }
    assert_eq!(stubs.telnyx_paths().len(), 1);
}

/// A full Telnyx setup, as the superuser's page sends it.
fn setup() -> Value {
    json!({
        "enabled": true, "credentialConnectionId": " 2001 ", "texmlAppId": "2002",
        "outboundVoiceProfileId": "2003", "billingGroupId": "", "telnyxAccountId": " acct-uuid-1 ",
        "aiAssistantId": "assistant-1",
        "fallbackTexmlUrl": "https://bin.test/fallback", "messagingProfileId": " profile-1 ",
        "minAppVersion": "1.4.0", "dailyCallLimit": 250,
        // Read-only, or the company admins' (PUT /v1/phone/admin/hours and
        // /recording): ignored.
        "webhookBase": "https://evil.test",
        "recordingMode": "both", "recordingNoticeUrl": "https://audio.test/notice.mp3",
        "businessHours": { "tz": "UTC", "days": [{ "day": 1, "start": "08:00", "end": "17:00" }] },
        "holidays": ["2026-12-25"],
    })
}

#[test_db::test(seed)]
async fn superusers_set_up_a_companys_telnyx(pool: MySqlPool) {
    sql(&pool, "UPDATE users SET is_superuser = 1 WHERE id = 103").await;
    let (server, _) = phone_server(pool.clone()).await;
    let path = "/v1/superuser/companies/100/telnyx";
    // Nothing saved yet; unknown company; not a superuser.
    server
        .get(path)
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/superuser/companies/999/telnyx")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    for session in [sessions::ADMIN, sessions::REP] {
        server
            .get(path)
            .with_session(session)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        server
            .put(path)
            .with_session(session)
            .json(&setup())
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .get(path)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // The first save makes the webhook token.
    let saved = server
        .put(path)
        .with_session(sessions::SUPER_ADMIN)
        .json(&setup())
        .await;
    saved.assert_status_ok();
    let token: String = sqlx::query_scalar(
        "SELECT webhook_token FROM telnyx_company_config WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    let expected = json!({
        "enabled": true, "credentialConnectionId": "2001", "texmlAppId": "2002",
        "outboundVoiceProfileId": "2003", "billingGroupId": null, "telnyxAccountId": "acct-uuid-1",
        "aiAssistantId": "assistant-1",
        "fallbackTexmlUrl": "https://bin.test/fallback", "recordingMode": "inbound",
        "recordingNoticeUrl": null, "messagingProfileId": "profile-1",
        "businessHours": null, "holidays": [],
        "minAppVersion": "1.4.0", "dailyCallLimit": 250,
        "webhookBase": format!("{PUBLIC_API}/v1/webhooks/telnyx/{token}"),
    });
    assert_eq!(saved.json::<Value>(), expected);
    let read = server.get(path).with_session(sessions::SUPER_ADMIN).await;
    read.assert_status_ok();
    assert_eq!(read.json::<Value>(), expected);
    // The account id is what the company's TeXML requests are checked for.
    let account: Option<String> = sqlx::query_scalar(
        "SELECT telnyx_account_id FROM telnyx_company_config WHERE company_id = 100",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(account.as_deref(), Some("acct-uuid-1"));
    let status_callback = |account: Option<&'static str>| {
        let mut form = vec![("CallSid", "v3:no-call"), ("CallStatus", "completed")];
        if let Some(account) = account {
            form.push(("AccountSid", account));
        }
        server
            .post(&format!("/v1/webhooks/telnyx/{token}/call-status"))
            .form(&form)
    };
    status_callback(None)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    status_callback(Some("acct-other"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    status_callback(Some("acct-uuid-1"))
        .await
        .assert_status_ok();
    // The admin page sees it set up.
    let status = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>()["config"]
        .clone();
    assert_eq!(
        status,
        json!({ "enabled": true, "recordingMode": "inbound", "configured": true })
    );
    // A second save keeps the token, and the hours the company's admins set.
    server
        .put("/v1/phone/admin/hours")
        .with_session(sessions::ADMIN)
        .json(&json!({ "businessHours": null, "holidays": ["2026-12-25"] }))
        .await
        .assert_status_ok();
    let mut off = setup();
    off["enabled"] = json!(false);
    let again = server
        .put(path)
        .with_session(sessions::SUPER_ADMIN)
        .json(&off)
        .await
        .json::<Value>();
    assert_eq!(
        (&again["enabled"], &again["holidays"], &again["webhookBase"]),
        (
            &json!(false),
            &json!(["2026-12-25"]),
            &expected["webhookBase"]
        )
    );
    // Light checks.
    for (field, value) in [
        ("texmlAppId", json!("20 02")),
        ("telnyxAccountId", json!("acct 1")),
        ("telnyxAccountId", json!("a".repeat(65))),
        ("aiAssistantId", json!("a".repeat(129))),
        ("messagingProfileId", json!("profile 1")),
        ("fallbackTexmlUrl", json!("ftp://bin.test")),
        ("minAppVersion", json!("latest")),
        ("dailyCallLimit", json!(-1)),
    ] {
        let mut bad = setup();
        bad[field] = value;
        server
            .put(path)
            .with_session(sessions::SUPER_ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .put("/v1/superuser/companies/999/telnyx")
        .with_session(sessions::SUPER_ADMIN)
        .json(&setup())
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let log = audit_log(&pool, 100).await;
    assert_eq!(log.len(), 3);
    assert_eq!(
        (&log[0].0, log[0].1.as_str(), log[0].2.as_deref()),
        (&Some(103), "telnyx_config_saved", Some("company:100"))
    );
    assert_eq!(
        (&log[0].3["created"], &log[2].3["created"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(log[2].3["enabled"], false);
}

#[test_db::test(seed)]
async fn company_admins_set_their_business_hours(pool: MySqlPool) {
    let (server, _) = phone_server(pool.clone()).await;
    let path = "/v1/phone/admin/hours";
    let hours = json!({
        "businessHours": { "tz": " America/Indiana/Indianapolis ",
                           "days": [{ "day": 1, "start": "08:00", "end": "17:00" }] },
        "holidays": ["2026-12-25", "2026-01-01", "2026-12-25"],
    });
    // Telnyx not set up yet.
    server
        .get(path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&hours)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    sql(
        &pool,
        "INSERT INTO telnyx_company_config (company_id, webhook_token) VALUES (100, 'hours-token')",
    )
    .await;
    // Not an admin, or not signed in.
    server
        .get(path)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put(path)
        .with_session(sessions::REP)
        .json(&hours)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get(path)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    let expected = json!({
        "businessHours": { "tz": "America/Indiana/Indianapolis",
                           "days": [{ "day": 1, "start": "08:00", "end": "17:00" }] },
        "holidays": ["2026-01-01", "2026-12-25"],
    });
    let saved = server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&hours)
        .await;
    saved.assert_status_ok();
    assert_eq!(saved.json::<Value>(), expected);
    let read = server.get(path).with_session(sessions::ADMIN).await;
    assert_eq!(read.json::<Value>(), expected);
    // Saving the same again works; switched off, no holidays.
    for body in [
        hours.clone(),
        json!({ "businessHours": null, "holidays": [] }),
    ] {
        server
            .put(path)
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status_ok();
    }
    assert_eq!(
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!({ "businessHours": null, "holidays": [] })
    );
    for (field, value) in [
        (
            "businessHours",
            json!({ "tz": "Mars/Base", "days": [{ "day": 1, "start": "08:00", "end": "17:00" }] }),
        ),
        (
            "businessHours",
            json!({ "tz": "UTC", "days": [{ "day": 1, "start": "17:00", "end": "08:00" }] }),
        ),
        ("holidays", json!(["2026-02-30"])),
    ] {
        let mut bad = hours.clone();
        bad[field] = value;
        server
            .put(path)
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let log = audit_log(&pool, 100).await;
    assert_eq!(log.len(), 3);
    assert_eq!(
        (&log[0].0, log[0].1.as_str(), log[0].2.as_deref()),
        (&Some(102), "business_hours_saved", Some("company:100"))
    );
    assert_eq!(log[0].3, expected);
}

#[test_db::test(seed)]
async fn company_admins_choose_what_is_recorded(pool: MySqlPool) {
    let (server, _) = phone_server(pool.clone()).await;
    let path = "/v1/phone/admin/recording";
    let both =
        json!({ "recordingMode": "both", "recordingNoticeUrl": " https://audio.test/notice.mp3 " });
    // Telnyx not set up yet.
    server
        .get(path)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&both)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    sql(
        &pool,
        "INSERT INTO telnyx_company_config (company_id, webhook_token) VALUES (100, 'rec-token')",
    )
    .await;
    // The table's default until the company chooses.
    assert_eq!(
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        json!({ "recordingMode": "inbound", "recordingNoticeUrl": null })
    );
    // Not an admin, or not signed in.
    server
        .put(path)
        .with_session(sessions::REP)
        .json(&both)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get(path)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    let expected =
        json!({ "recordingMode": "both", "recordingNoticeUrl": "https://audio.test/notice.mp3" });
    let saved = server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&both)
        .await;
    saved.assert_status_ok();
    assert_eq!(saved.json::<Value>(), expected);
    assert_eq!(
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>(),
        expected
    );
    // The page's status shows it.
    let status = server
        .get("/v1/phone/admin/numbers")
        .with_session(sessions::ADMIN)
        .await
        .json::<Value>()["config"]["recordingMode"]
        .clone();
    assert_eq!(status, json!("both"));
    // Off, spoken notice.
    let off = server
        .put(path)
        .with_session(sessions::ADMIN)
        .json(&json!({ "recordingMode": "off", "recordingNoticeUrl": "  " }))
        .await;
    assert_eq!(
        off.json::<Value>(),
        json!({ "recordingMode": "off", "recordingNoticeUrl": null })
    );
    for bad in [
        json!({ "recordingMode": "all" }),
        json!({ "recordingMode": "both", "recordingNoticeUrl": "notice.mp3" }),
    ] {
        let status = server
            .put(path)
            .with_session(sessions::ADMIN)
            .json(&bad)
            .await
            .status_code();
        assert!(status.is_client_error(), "{bad}: {status}");
    }
    let log = audit_log(&pool, 100).await;
    assert_eq!(log.len(), 2);
    assert_eq!(
        (&log[0].0, log[0].1.as_str(), log[0].2.as_deref()),
        (&Some(102), "recording_saved", Some("company:100"))
    );
    assert_eq!(log[0].3, expected);
}
