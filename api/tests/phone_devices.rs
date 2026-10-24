//! Granite Phone devices: registering a phone (and its Telnyx credential),
//! login JWTs and their limits, push tokens (`routes/phone/devices.rs`).
//!
//! Fixture phones use n = 1..=20 (sessions 950001+, devices 960001+).

mod common;
mod phone_common;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, Utc};
use phone_common::{
    HEADER, JWT, JWT_EXPIRES, add_phone, add_session, assert_device_only, enable_calling,
    phone_server, revoke_session, sql, token,
};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::atomic::Ordering;

fn registration(push_token: Option<&str>) -> Value {
    json!({
        "platform": "android",
        "appVersion": "1.4.0",
        "deviceName": "Rita's Pixel 9",
        "pushToken": push_token,
    })
}

/// Seconds between `at` and 30 days from now.
fn off_30_days(at: &str) -> i64 {
    let at = DateTime::parse_from_rfc3339(at)
        .unwrap()
        .with_timezone(&Utc);
    ((Utc::now() + Duration::days(30)) - at).num_seconds().abs()
}

type DeviceRow = (
    i64,
    i32,
    i32,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn device_row(pool: &MySqlPool, id: i64) -> DeviceRow {
    sqlx::query_as(
        "SELECT CAST(device_session_id AS SIGNED), user_id, company_id, CAST(platform AS CHAR),
                device_name, app_version, telnyx_credential_id, sip_username, push_token
           FROM phone_devices WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_phone_registers_once_and_gets_its_credential(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let session = add_session(&pool, 1, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;

    let first = server
        .post("/v1/phone/devices")
        .add_header(HEADER, token(1))
        .json(&registration(Some(" fcm-1 ")))
        .await;
    first.assert_status(StatusCode::CREATED);
    let body: Value = first.json();
    let id = body["id"].as_i64().unwrap();
    let created_at = body["createdAt"].as_str().unwrap().to_string();
    assert_eq!(
        body,
        json!({ "id": id, "sipUsername": "gencrednew1", "pushTokenSet": true, "createdAt": created_at })
    );
    // Telnyx made the phone's credential on the company's connection, for
    // 30 days, named by ids only.
    let seen = stubs.telnyx();
    assert_eq!(stubs.telnyx_paths(), ["POST /telephony_credentials"]);
    let request = &seen[0].2;
    assert_eq!(
        (&request["connection_id"], &request["name"], &request["tag"]),
        (
            &json!("conn-100"),
            &json!(format!("phone-{id}")),
            &json!("company-100")
        )
    );
    assert!(off_30_days(request["expires_at"].as_str().unwrap()) < 120);
    assert_eq!(
        device_row(&pool, id).await,
        (
            session,
            100,
            100,
            "android".into(),
            "Rita's Pixel 9".into(),
            Some("1.4.0".into()),
            Some("cred-new-1".into()),
            Some("gencrednew1".into()),
            Some("fcm-1".into())
        )
    );

    // Registering again updates the same phone: no second credential, and a
    // null push token keeps the one we have.
    let again = server
        .post("/v1/phone/devices")
        .add_header(HEADER, token(1))
        .json(&json!({ "platform": "ios", "appVersion": " 1.5.0 ", "deviceName": "Rita's iPhone", "pushToken": null }))
        .await;
    again.assert_status_ok();
    assert_eq!(
        again.json::<Value>(),
        json!({ "id": id, "sipUsername": "gencrednew1", "pushTokenSet": true, "createdAt": created_at })
    );
    assert_eq!(stubs.telnyx().len(), 1);
    let row = device_row(&pool, id).await;
    assert_eq!(
        (
            row.3.as_str(),
            row.4.as_str(),
            row.5.as_deref(),
            row.8.as_deref()
        ),
        ("ios", "Rita's iPhone", Some("1.5.0"), Some("fcm-1"))
    );
    // The bootstrap shows it.
    let bootstrap: Value = server
        .get("/v1/phone/bootstrap")
        .add_header(HEADER, token(1))
        .await
        .json();
    assert_eq!(
        bootstrap["device"],
        json!({ "id": id, "sipUsername": "gencrednew1", "pushTokenSet": true, "createdAt": created_at })
    );

    // Bad bodies.
    for body in [
        json!({ "platform": "android", "appVersion": "1", "deviceName": "  ", "pushToken": null }),
        json!({ "platform": "windows", "appVersion": "1", "deviceName": "x", "pushToken": null }),
        json!({ "platform": "android", "appVersion": "1", "deviceName": "x", "pushToken": "x".repeat(513) }),
    ] {
        server
            .post("/v1/phone/devices")
            .add_header(HEADER, token(1))
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}

#[test_db::test(seed)]
async fn registering_twice_at_once_makes_one_phone(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    add_session(&pool, 3, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let register = || {
        server
            .post("/v1/phone/devices")
            .add_header(HEADER, token(3))
            .json(&registration(Some("fcm-3")))
    };
    let (a, b, c) = tokio::join!(register(), register(), register());
    for response in [&a, &b, &c] {
        assert!(
            response.status_code().is_success(),
            "{}",
            response.status_code()
        );
    }
    let ids: Vec<i64> = [a, b, c]
        .iter()
        .map(|response| response.json::<Value>()["id"].as_i64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|pair| pair[0] == pair[1]), "{ids:?}");
    let phones: (i64, i64) = sqlx::query_as(
        "SELECT CAST(COUNT(*) AS SIGNED), CAST(COUNT(telnyx_credential_id) AS SIGNED)
           FROM phone_devices WHERE device_session_id = 950003",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phones, (1, 1));
    // A revoked phone is not brought back.
    sql(
        &pool,
        "UPDATE phone_devices SET revoked_at = UTC_TIMESTAMP() WHERE device_session_id = 950003",
    )
    .await;
    let revoked = register().await;
    revoked.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(
        revoked.json::<Value>(),
        json!({ "error": "device_revoked" })
    );
}

#[test_db::test(seed)]
async fn while_calling_is_off_a_phone_has_no_sip_user(pool: MySqlPool) {
    add_session(&pool, 2, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let register = || {
        server
            .post("/v1/phone/devices")
            .add_header(HEADER, token(2))
            .json(&registration(None))
    };
    // A company without a Telnyx setup.
    let first = register().await;
    first.assert_status(StatusCode::CREATED);
    let body: Value = first.json();
    assert_eq!(body["sipUsername"], Value::Null);
    assert_eq!(body["pushTokenSet"], false);
    let id = body["id"].as_i64().unwrap();
    // One that is switched off.
    enable_calling(&pool, 100, "enabled = 0").await;
    let off = register().await;
    off.assert_status_ok();
    assert_eq!(off.json::<Value>()["sipUsername"], Value::Null);
    assert!(stubs.telnyx().is_empty());
    // When Telnyx fails, the phone stays registered without a credential.
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 1 WHERE company_id = 100",
    )
    .await;
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    register().await.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(device_row(&pool, id).await.6, None);
    // Once Telnyx answers, the next registration gets it.
    stubs.telnyx_down.store(false, Ordering::SeqCst);
    let on = register().await;
    on.assert_status_ok();
    assert_eq!(
        on.json::<Value>(),
        json!({ "id": id, "sipUsername": "gencrednew1", "pushTokenSet": false, "createdAt": body["createdAt"] })
    );
}

async fn push_tokens(pool: &MySqlPool, ids: &[i64]) -> Vec<Option<String>> {
    let mut tokens = Vec::new();
    for id in ids {
        let token: Option<String> =
            sqlx::query_scalar("SELECT push_token FROM phone_devices WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        tokens.push(token);
    }
    tokens
}

#[test_db::test(seed)]
async fn a_push_token_belongs_to_one_phone(pool: MySqlPool) {
    // An older sign-in on the same phone (push-3), and another user's phone.
    let old = add_phone(&pool, 3, 100, 100).await;
    let marks = add_phone(&pool, 5, 101, 100).await;
    add_session(&pool, 4, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    let new: Value = server
        .post("/v1/phone/devices")
        .add_header(HEADER, token(4))
        .json(&registration(Some("push-3")))
        .await
        .json();
    let new = new["id"].as_i64().unwrap();
    assert_eq!(
        push_tokens(&pool, &[old, new, marks]).await,
        [None, Some("push-3".into()), Some("push-5".into())]
    );
    // The token changes.
    server
        .put(&format!("/v1/phone/devices/{new}/push-token"))
        .add_header(HEADER, token(4))
        .json(&json!({ "pushToken": " fcm-new " }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(push_tokens(&pool, &[new]).await, [Some("fcm-new".into())]);
    // Another session's phone is not this one's to change.
    for id in [old, marks, 999_999] {
        server
            .put(&format!("/v1/phone/devices/{id}/push-token"))
            .add_header(HEADER, token(4))
            .json(&json!({ "pushToken": "fcm-x" }))
            .await
            .assert_status_not_found();
    }
    for bad in [
        json!({ "pushToken": "  " }),
        json!({ "pushToken": "x".repeat(513) }),
        json!({}),
    ] {
        server
            .put(&format!("/v1/phone/devices/{new}/push-token"))
            .add_header(HEADER, token(4))
            .json(&bad)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        push_tokens(&pool, &[new, marks]).await,
        [Some("fcm-new".into()), Some("push-5".into())]
    );
}

#[test_db::test(seed)]
async fn login_tokens_are_minted_and_limited(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let device = add_phone(&pool, 6, 100, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    // Native code sends `{}`.
    let mint = || {
        server
            .post(&format!("/v1/phone/devices/{device}/token"))
            .add_header(HEADER, token(6))
            .json(&json!({}))
    };
    let first = mint().await;
    first.assert_status_ok();
    assert_eq!(
        first.json::<Value>(),
        json!({ "jwt": JWT, "expiresAt": JWT_EXPIRES })
    );
    // The credential is extended to 30 days, then the JWT minted.
    assert_eq!(
        stubs.telnyx_paths(),
        [
            "PATCH /telephony_credentials/cred-6",
            "POST /telephony_credentials/cred-6/token"
        ]
    );
    assert!(off_30_days(stubs.telnyx()[0].2["expires_at"].as_str().unwrap()) < 120);
    let extended: DateTime<Utc> =
        sqlx::query_scalar("SELECT credential_expires_at FROM phone_devices WHERE id = ?")
            .bind(device)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(off_30_days(&extended.to_rfc3339()) < 120);

    // One a minute.
    let limited = mint().await;
    limited.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let body: Value = limited.json();
    let wait = body["retryAfterSecs"].as_u64().unwrap();
    assert_eq!(
        body,
        json!({ "error": "rate_limited", "retryAfterSecs": wait })
    );
    assert!((55..=60).contains(&wait), "{wait}");
    assert_eq!(limited.headers()["retry-after"], wait.to_string());

    // Six an hour: the sixth of the window is the last.
    let window = |minutes: i64, count: i32| {
        format!(
            "UPDATE phone_devices SET last_token_at = UTC_TIMESTAMP() - INTERVAL 2 MINUTE,
                    token_window_start = UTC_TIMESTAMP() - INTERVAL {minutes} MINUTE,
                    token_window_count = {count} WHERE id = {device}"
        )
    };
    sql(&pool, &window(50, 5)).await;
    mint().await.assert_status_ok();
    sql(&pool, &window(50, 6)).await;
    let hourly = mint().await;
    hourly.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = hourly.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((590..=600).contains(&wait), "{wait}");
    // An hour after the window started, a new one begins.
    sql(&pool, &window(61, 6)).await;
    mint().await.assert_status_ok();
    let count: i32 =
        sqlx::query_scalar("SELECT token_window_count FROM phone_devices WHERE id = ?")
            .bind(device)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[test_db::test(seed)]
async fn login_tokens_need_calling_this_phone_and_a_current_app(pool: MySqlPool) {
    let device = add_phone(&pool, 7, 100, 100).await;
    let marks = add_phone(&pool, 8, 101, 100).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let mint = |id: i64| {
        server
            .post(&format!("/v1/phone/devices/{id}/token"))
            .add_header(HEADER, token(7))
            .json(&json!({}))
    };
    let off = mint(device).await;
    off.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(off.json::<Value>(), json!({ "error": "calling_disabled" }));
    enable_calling(&pool, 100, "min_app_version = '1.2.0'").await;
    // Another session's phone, or none.
    mint(marks).await.assert_status_not_found();
    mint(999_999).await.assert_status_not_found();
    // An app older than the company's minimum.
    sql(
        &pool,
        &format!("UPDATE phone_devices SET app_version = '1.1.9' WHERE id = {device}"),
    )
    .await;
    let old = mint(device).await;
    old.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(old.json::<Value>(), json!({ "error": "update_required" }));
    assert!(stubs.telnyx().is_empty());
    // Updated from the store: the version the phone runs now counts, and is saved.
    server
        .post(&format!("/v1/phone/devices/{device}/token"))
        .add_header(HEADER, token(7))
        .add_header("X-Granite-App-Version", "1.3.0")
        .json(&json!({}))
        .await
        .assert_status_ok();
    let saved: Option<String> =
        sqlx::query_scalar("SELECT app_version FROM phone_devices WHERE id = ?")
            .bind(device)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(saved.as_deref(), Some("1.3.0"));
    stubs.clear_telnyx();
    sql(
        &pool,
        &format!(
            "UPDATE phone_devices SET last_token_at = NULL, token_window_start = NULL,
                    token_window_count = 0 WHERE id = {device}"
        ),
    )
    .await;
    sql(
        &pool,
        &format!("UPDATE phone_devices SET app_version = '1.2.0' WHERE id = {device}"),
    )
    .await;
    // When Telnyx fails the attempt does not count.
    stubs.telnyx_down.store(true, Ordering::SeqCst);
    mint(device).await.assert_status(StatusCode::BAD_GATEWAY);
    stubs.telnyx_down.store(false, Ordering::SeqCst);
    mint(device).await.assert_status_ok();
    // A phone without a credential gets one first.
    sql(
        &pool,
        &format!(
            "UPDATE phone_devices SET telnyx_credential_id = NULL, sip_username = NULL,
                    last_token_at = NULL WHERE id = {device}"
        ),
    )
    .await;
    stubs.clear_telnyx();
    mint(device).await.assert_status_ok();
    assert_eq!(
        stubs.telnyx_paths(),
        [
            "POST /telephony_credentials",
            "PATCH /telephony_credentials/cred-new-1",
            "POST /telephony_credentials/cred-new-1/token"
        ]
    );
    let sip: Option<String> =
        sqlx::query_scalar("SELECT sip_username FROM phone_devices WHERE id = ?")
            .bind(device)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sip.as_deref(), Some("gencrednew1"));
}

#[test_db::test(seed)]
async fn device_routes_take_only_a_live_device_token(pool: MySqlPool) {
    let device = add_phone(&pool, 9, 100, 100).await;
    add_phone(&pool, 10, 100, 100).await;
    revoke_session(&pool, 10).await;
    // Another company's user cannot reach this phone either.
    add_phone(&pool, 11, 106, 101).await;
    let (server, _) = phone_server(pool.clone()).await;
    assert_device_only(
        &server,
        10,
        &[
            (Method::POST, "/v1/phone/devices".to_string()),
            (Method::POST, format!("/v1/phone/devices/{device}/token")),
            (
                Method::PUT,
                format!("/v1/phone/devices/{device}/push-token"),
            ),
        ],
    )
    .await;
    server
        .put(&format!("/v1/phone/devices/{device}/push-token"))
        .add_header(HEADER, token(11))
        .json(&json!({ "pushToken": "fcm-x" }))
        .await
        .assert_status_not_found();
    server
        .post(&format!("/v1/phone/devices/{device}/token"))
        .add_header(HEADER, token(11))
        .json(&json!({}))
        .await
        .assert_status_not_found();
}
