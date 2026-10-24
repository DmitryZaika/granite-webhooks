//! Granite Phone start-up: `GET /v1/phone/bootstrap`
//! (`routes/phone/bootstrap.rs`).
//!
//! Fixture phones use n = 61..=80; numbers 9730+; calls 980200+.

mod common;
mod phone_common;

use api::phone::PhoneConfig;
use axum::http::Method;
use chrono::{DateTime, Utc};
use phone_common::{
    APP_URL, HEADER, add_call, add_number, add_phone, add_session, assert_device_only, assign,
    enable_calling, phone_server, phone_server_with, revoke_session, set_settings, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;

/// Every day, all day, in Indianapolis.
fn always() -> Value {
    json!({ "tz": "America/Indiana/Indianapolis", "days": (0..7)
        .map(|day| json!({ "day": day, "start": "00:00", "end": "24:00" }))
        .collect::<Vec<_>>() })
}

/// The bootstrap of phone `n`, with its server time and contacts ETag
/// checked and taken out.
async fn bootstrap(server: &axum_test::TestServer, n: u32) -> (Value, String) {
    let response = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(n))
        .await;
    response.assert_status_ok();
    let mut body: Value = response.json();
    let server_time = DateTime::parse_from_rfc3339(body["serverTime"].as_str().unwrap()).unwrap();
    assert!(
        (Utc::now() - server_time.with_timezone(&Utc))
            .num_seconds()
            .abs()
            < 60
    );
    let etag = body["contacts"]["etag"].as_str().unwrap().to_string();
    assert_eq!(etag.len(), 34, "{etag}");
    body["serverTime"] = json!("<now>");
    body["contacts"]["etag"] = json!("<etag>");
    (body, etag)
}

#[test_db::test(seed)]
async fn the_bootstrap_has_everything_the_app_needs(pool: MySqlPool) {
    enable_calling(
        &pool,
        100,
        "recording_mode = 'both', min_app_version = '1.2.0'",
    )
    .await;
    add_number(&pool, 9731, 100, "+13175550141", "direct", "Rita direct").await;
    add_number(&pool, 9732, 100, "+13175550142", "shared", "Sales").await;
    add_number(&pool, 9733, 100, "+13175550140", "main", "Main line").await;
    assign(&pool, 9731, 100, true, true).await;
    assign(&pool, 9732, 100, true, true).await;
    assign(&pool, 9732, 101, true, true).await;
    assign(&pool, 9733, 100, false, false).await;
    set_settings(
        &pool,
        100,
        100,
        &format!(
            "default_caller_number_id = 9732, ai_notes = 0, ring_timeout_secs = 30,
             working_hours = '{}', cell_number = '+13175550177', cell_verified_at = UTC_TIMESTAMP(),
             cell_fallback = 1",
            always()
        ),
    )
    .await;
    sql(
        &pool,
        "INSERT INTO phone_notice_acks (user_id, notice, version) VALUES (100, 'recording', 1)",
    )
    .await;
    add_phone(&pool, 61, 100, 100).await;
    // Unread: missed and canceled calls of my direct line, a voicemail, an
    // AI message of the sales line that rings me. Not: a seen one, a
    // hidden one, the main line that does not ring me, an answered one, my
    // outgoing call, a test call, company 101's.
    let incoming = [
        (980_201, "missed", 9731, ""),
        (980_202, "voicemail", 9731, ""),
        (980_203, "ai_message", 9732, ""),
        (980_204, "missed", 9731, "seen"),
        (980_205, "missed", 9731, "hidden"),
        (980_206, "no_answer", 9733, ""),
        (980_209, "canceled", 9731, ""),
        (980_210, "answered", 9731, "answered"),
    ];
    // Two days ago: unread counts look 30 days back.
    let recent = "started_at = UTC_TIMESTAMP() - INTERVAL 2 DAY";
    for (id, status, number, mark) in incoming {
        let extra = if mark == "answered" {
            format!(
                "number_id = {number}, {recent}, answered_at = UTC_TIMESTAMP() - INTERVAL 2 DAY"
            )
        } else {
            format!("number_id = {number}, {recent}")
        };
        add_call(
            &pool,
            id,
            100,
            None,
            "inbound",
            status,
            "+13175550199",
            "2026-10-17 10:00:00",
            &extra,
        )
        .await;
        match mark {
            "seen" => sql(&pool, &format!("INSERT INTO phone_call_views (call_id, user_id, seen_at) VALUES ({id}, 100, UTC_TIMESTAMP())")).await,
            "hidden" => sql(&pool, &format!("INSERT INTO phone_call_views (call_id, user_id, hidden_at) VALUES ({id}, 100, UTC_TIMESTAMP())")).await,
            _ => {}
        }
    }
    add_call(
        &pool,
        980_207,
        101,
        None,
        "inbound",
        "missed",
        "+13175550199",
        "2026-10-17 10:00:00",
        recent,
    )
    .await;
    add_call(
        &pool,
        980_208,
        100,
        Some(100),
        "outbound",
        "no_answer",
        "+13175550199",
        "2026-10-17 10:00:00",
        recent,
    )
    .await;
    add_call(
        &pool,
        980_211,
        100,
        Some(100),
        "inbound",
        "missed",
        "+13175550141",
        "2026-10-17 10:00:00",
        &format!("our_number = '+13175550141', number_id = 9731, {recent}"),
    )
    .await;
    // A missed call of more than 30 days ago no longer counts.
    add_call(
        &pool,
        980_212,
        100,
        None,
        "inbound",
        "missed",
        "+13175550199",
        "2026-10-17 10:00:00",
        "number_id = 9731, started_at = UTC_TIMESTAMP() - INTERVAL 31 DAY",
    )
    .await;
    let (server, _) = phone_server(pool.clone()).await;
    let (body, _) = bootstrap(&server, 61).await;
    assert_eq!(
        body,
        json!({
            "serverTime": "<now>",
            "minAppVersion": "1.2.0",
            "callingEnabled": true,
            "user": { "id": 100, "name": "Rita Rep", "email": "rep@acme.test", "companyId": 100,
                      "companyName": "Acme Stone", "isAdmin": false, "portal": null },
            "device": { "id": 960_061, "sipUsername": "gencred61", "pushTokenSet": true,
                        "createdAt": "2026-10-01T09:00:00.000Z" },
            "callerIds": [
                { "numberId": 9731, "e164": "+13175550141", "label": "Rita direct", "kind": "direct" },
                { "numberId": 9732, "e164": "+13175550142", "label": "Sales", "kind": "shared" },
            ],
            "settings": { "defaultCallerNumberId": 9732, "aiNotes": false, "workingHours": always(),
                          "cellNumber": "+13175550177", "cellVerified": true, "cellFallback": true,
                          "ringTimeoutSecs": 30, "forwardAll": false, "forwardAllTo": "voicemail", "forwardAllNumber": null,
                          "displayNumber": "caller", "unknownShowMine": false, "announceDirect": false,
                          "announceShared": false, "announceLineName": true, "requirePin": false,
                          "holdMusic": "calm", "holdMusicUrl": null, "holdMusicName": null },
            "availability": { "status": "available", "dndUntil": null, "dndIndefinite": false },
            "recordingMode": "both",
            "notices": { "recordingVersion": 1, "e911Version": 1, "acceptedRecordingVersion": 1,
                         "acceptedE911Version": null },
            "contacts": { "count": 12, "etag": "<etag>" },
            "unread": { "missed": 2, "voicemail": 1, "aiMessage": 1 },
            "webBaseUrl": APP_URL,
            "supportEmail": "support@granite-manager.com",
            "texting": { "enabled": false, "unread": 0 },
        })
    );
    // Seeing them clears the badge.
    server
        .post("/v1/phone/calls/seen")
        .add_header(HEADER, token(61))
        .json(&json!({ "ids": [980_201, 980_203] }))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    let (body, _) = bootstrap(&server, 61).await;
    assert_eq!(
        body["unread"],
        json!({ "missed": 1, "voicemail": 1, "aiMessage": 0 })
    );
    // Mark rings on the sales line too: its AI message is his to see.
    add_session(&pool, 62, 101, 100).await;
    let (marks, _) = bootstrap(&server, 62).await;
    assert_eq!(
        marks["unread"],
        json!({ "missed": 0, "voicemail": 0, "aiMessage": 1 })
    );
    assert_eq!(marks["device"], Value::Null);
}

#[test_db::test(seed)]
async fn a_new_user_without_calling_gets_the_defaults(pool: MySqlPool) {
    add_session(&pool, 63, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    let defaults = json!({
        "serverTime": "<now>",
        "minAppVersion": null,
        "callingEnabled": false,
        "user": { "id": 106, "name": "Olga Other", "email": "rep@other.test", "companyId": 101,
                  "companyName": "Other Granite Co", "isAdmin": false, "portal": null },
        "device": null,
        "callerIds": [],
        "settings": { "defaultCallerNumberId": null, "aiNotes": true, "workingHours": null,
                      "cellNumber": null, "cellVerified": false, "cellFallback": false,
                      "ringTimeoutSecs": 25, "forwardAll": false, "forwardAllTo": "voicemail", "forwardAllNumber": null,
                          "displayNumber": "caller", "unknownShowMine": false, "announceDirect": false,
                          "announceShared": false, "announceLineName": true, "requirePin": false,
                          "holdMusic": "calm", "holdMusicUrl": null, "holdMusicName": null },
        "availability": { "status": "available", "dndUntil": null, "dndIndefinite": false },
        "recordingMode": "inbound",
        "notices": { "recordingVersion": 1, "e911Version": 1, "acceptedRecordingVersion": null,
                     "acceptedE911Version": null },
        "contacts": { "count": 1, "etag": "<etag>" },
        "unread": { "missed": 0, "voicemail": 0, "aiMessage": 0 },
        "webBaseUrl": APP_URL,
        "supportEmail": "support@granite-manager.com",
        "texting": { "enabled": false, "unread": 0 },
    });
    let (body, etag) = bootstrap(&server, 63).await;
    assert_eq!(body, defaults);
    // The same index gives the same ETag; another user's differs (`mine`).
    assert_eq!(bootstrap(&server, 63).await.1, etag);
    add_session(&pool, 64, 103, 101).await;
    let (sam, sams_etag) = bootstrap(&server, 64).await;
    assert_ne!(sams_etag, etag);
    // A super admin of the company is an admin in the app.
    assert_eq!(
        sam["user"],
        json!({ "id": 103, "name": "Sam Super", "email": "super@other.test", "companyId": 101,
                "companyName": "Other Granite Co", "isAdmin": true, "portal": null })
    );
    // An unverified cell is no cell; a switched-off company calls nothing.
    set_settings(
        &pool,
        106,
        101,
        "cell_number = '+13175550177', cell_fallback = 1",
    )
    .await;
    enable_calling(&pool, 101, "enabled = 0").await;
    let (body, _) = bootstrap(&server, 63).await;
    assert_eq!(body["settings"], defaults["settings"]);
    assert_eq!(body["callingEnabled"], false);
    assert_eq!(body["recordingMode"], "inbound");
}

#[test_db::test(seed)]
async fn the_minimum_app_version_falls_back_to_the_servers(pool: MySqlPool) {
    add_session(&pool, 65, 106, 101).await;
    add_session(&pool, 66, 100, 100).await;
    enable_calling(&pool, 100, "min_app_version = '2.0.0'").await;
    let (server, _) = phone_server_with(pool.clone(), |state| {
        state.with_phone_config(PhoneConfig {
            min_app_version: Some(Arc::from("1.1.0")),
            ..PhoneConfig::default()
        })
    })
    .await;
    assert_eq!(bootstrap(&server, 65).await.0["minAppVersion"], "1.1.0");
    assert_eq!(bootstrap(&server, 66).await.0["minAppVersion"], "2.0.0");
}

#[test_db::test(seed)]
async fn the_bootstrap_takes_only_a_live_device_token(pool: MySqlPool) {
    add_phone(&pool, 67, 100, 100).await;
    revoke_session(&pool, 67).await;
    // A deleted user's and a non-employee's sessions are out too.
    add_session(&pool, 68, 105, 100).await;
    add_session(&pool, 69, 104, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        67,
        &[(Method::GET, "/v1/phone/bootstrap".to_string())],
    )
    .await;
    for n in [68, 69] {
        server
            .get("/v1/phone/bootstrap")
            .add_header(HEADER, token(n))
            .await
            .assert_status_unauthorized();
    }
}
