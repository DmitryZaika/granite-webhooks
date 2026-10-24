//! Granite Phone sign-in: device sessions, the `X-Granite-Device` header and
//! signing phones out (`routes/device_sessions.rs`, `auth/device.rs`).
//!
//! Fixture device sessions use ids 900100+ and phone devices 900200+; their
//! tokens are 43 digits (`token(n)`), stored as their SHA-256 like real ones.

mod common;

use api::app;
use api::auth::device::token_hash;
use api::state::AppState;
use api::telnyx::Telnyx;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::delete;
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{DateTime, Months, SecondsFormat, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const PASSWORD: &str = "granite-local";
const HEADER: &str = "x-granite-device";
const TELNYX_KEY: &str = "test-telnyx-key";

/// A well-formed device token for fixture `n` (43 characters).
fn token(n: u32) -> String {
    format!("{n:0>43}")
}

/// Credentials the stand-in for Telnyx deleted.
type Deleted = Arc<Mutex<Vec<String>>>;

/// The API with Telnyx pointed at a stub that deletes credentials (and
/// fails for `cred-fail`).
async fn server(pool: MySqlPool) -> (TestServer, Deleted) {
    let deleted: Deleted = Arc::default();
    let log = Arc::clone(&deleted);
    let router = Router::new().route(
        "/telephony_credentials/{id}",
        delete(move |Path(id): Path<String>, headers: HeaderMap| {
            let log = Arc::clone(&log);
            async move {
                assert_eq!(headers["authorization"], format!("Bearer {TELNYX_KEY}"));
                if id == "cred-fail" {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(
                            json!({ "errors": [{ "code": "10007", "title": "Unexpected error" }] }),
                        ),
                    );
                }
                log.lock().unwrap().push(id.clone());
                (
                    StatusCode::OK,
                    Json(json!({ "data": { "id": id, "record_type": "credential" } })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(Some(TELNYX_KEY), &format!("http://{address}")));
    (TestServer::new(app(state)), deleted)
}

/// Inserts a device session; `extra` sets columns over the defaults (a live
/// android session of `user_id` in `company_id`, signed in on 2026-10-01).
async fn add_session(
    pool: &MySqlPool,
    id: i64,
    user_id: i32,
    company_id: i32,
    n: u32,
    extra: &str,
) {
    sqlx::query(
        "INSERT INTO device_sessions
           (id, user_id, company_id, token_hash, device_name, platform, app_version,
            created_at, last_seen_at, expires_at)
         VALUES (?, ?, ?, ?, 'Test phone', 'android', '1.0.0', '2026-10-01 12:00:00',
                 UTC_TIMESTAMP() - INTERVAL 1 MINUTE, UTC_TIMESTAMP() + INTERVAL 11 MONTH)",
    )
    .bind(id)
    .bind(user_id)
    .bind(company_id)
    .bind(token_hash(&token(n)))
    .execute(pool)
    .await
    .unwrap();
    if !extra.is_empty() {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE device_sessions SET {extra} WHERE id = {id}"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

fn sign_in_body(email: &str, password: &str) -> Value {
    json!({
        "email": email,
        "password": password,
        "deviceName": "Rita's Pixel 9",
        "platform": "android",
        "appVersion": "1.4.0",
    })
}

fn js(date: DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The call key sign-in hands out for device session `session_id`, made
/// apart from the API: base64url of HMAC-SHA256 keyed with the
/// `SESSION_SECRET` over `granite-phone-call-key:{session id}`.
fn expected_call_key(session_id: i64) -> String {
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("granite-phone-call-key:{session_id}").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

#[test_db::test(seed)]
async fn a_phone_signs_in_with_the_crm_password(pool: MySqlPool) {
    let (server, _) = server(pool.clone()).await;
    let response = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("  REP@acme.test ", PASSWORD))
        .await;
    response.assert_status(StatusCode::CREATED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body: Value = response.json();
    let token = body["token"].as_str().unwrap().to_string();
    let session_id = body["sessionId"].as_i64().unwrap();
    let expires_at = body["expiresAt"].as_str().unwrap().to_string();
    // The call key is derived from the server's secret and the session id.
    let call_key = expected_call_key(session_id);
    assert_eq!(
        body,
        json!({
            "token": token,
            "callKey": call_key,
            "sessionId": session_id,
            "expiresAt": expires_at,
            "user": {
                "id": 100, "name": "Rita Rep", "email": "rep@acme.test",
                "companyId": 100, "companyName": "Acme Stone", "isAdmin": false,
            },
        })
    );
    for shown in [&token, &call_key] {
        assert_eq!(shown.len(), 43);
        assert!(
            shown
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }
    assert_ne!(token, call_key);
    // Twelve months out, to the second.
    let expires = DateTime::parse_from_rfc3339(&expires_at)
        .unwrap()
        .with_timezone(&Utc);
    let year = Utc::now().checked_add_months(Months::new(12)).unwrap();
    assert!((year - expires).num_seconds().abs() < 60, "{expires_at}");
    assert!(expires_at.ends_with(".000Z"));
    // Only the hash is stored.
    let row: (
        String,
        String,
        String,
        Option<String>,
        i32,
        i32,
        DateTime<Utc>,
    ) = sqlx::query_as(
        "SELECT token_hash, device_name, CAST(platform AS CHAR), app_version, user_id, company_id,
                expires_at
           FROM device_sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            token_hash(&token),
            "Rita's Pixel 9".to_string(),
            "android".to_string(),
            Some("1.4.0".to_string()),
            100,
            100,
            expires
        )
    );
    // The token signs the app in.
    let devices: Value = server
        .get("/v1/me/devices")
        .add_header(HEADER, token.clone())
        .await
        .json();
    assert_eq!(devices["devices"][0]["id"], session_id);
    assert_eq!(devices["devices"][0]["current"], true);
    // Admins are admins in the app too. Each sign-in has a call key of its
    // own.
    let admin: Value = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("admin@acme.test", PASSWORD))
        .await
        .json();
    assert_eq!(admin["user"]["isAdmin"], true);
    let admin_session = admin["sessionId"].as_i64().unwrap();
    assert_ne!(admin_session, session_id);
    assert_eq!(admin["callKey"], expected_call_key(admin_session));
    assert_ne!(admin["callKey"], json!(call_key));
}

#[test_db::test(seed)]
async fn wrong_credentials_are_invalid_credentials(pool: MySqlPool) {
    let (server, _) = server(pool.clone()).await;
    // A wrong password, an unknown email and a deleted user look the same.
    for email in ["rep@acme.test", "nobody@acme.test", "gone@acme.test"] {
        let password = if email == "rep@acme.test" {
            "wrong-password"
        } else {
            PASSWORD
        };
        let response = server
            .post("/v1/auth/device-sessions")
            .json(&sign_in_body(email, password))
            .await;
        response.assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.json::<Value>(),
            json!({ "error": "invalid_credentials" })
        );
    }
    let (attempts, sessions): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM login_attempts), (SELECT COUNT(*) FROM device_sessions)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((attempts, sessions), (3, 0));
    // Missing or unknown fields are a 400.
    for body in [
        json!({ "email": "rep@acme.test", "password": PASSWORD, "platform": "android", "appVersion": "1" }),
        json!({ "email": "rep@acme.test", "password": PASSWORD, "deviceName": "x", "platform": "windows", "appVersion": "1" }),
        json!({ "email": "rep@acme.test", "password": PASSWORD, "deviceName": "  ", "platform": "ios", "appVersion": "1" }),
        json!({ "email": " ", "password": PASSWORD, "deviceName": "x", "platform": "ios", "appVersion": "1" }),
    ] {
        server
            .post("/v1/auth/device-sessions")
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}

#[test_db::test(seed)]
async fn a_deleted_company_signs_no_phone_in(pool: MySqlPool) {
    let (server, _) = server(pool.clone()).await;
    add_session(&pool, 900_150, 100, 100, 150, "").await;
    let signed_in = |n: u32| server.get("/v1/me/devices").add_header(HEADER, token(n));
    signed_in(150).await.assert_status_ok();
    sqlx::query("UPDATE company SET deleted_at = NOW() WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    // The live session of the deleted company no longer works, and no new
    // sign-in is made.
    signed_in(150).await.assert_status_unauthorized();
    let response = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("rep@acme.test", PASSWORD))
        .await;
    response.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "no_phone_access" })
    );
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
    // The phone job finds it among the sign-ins that lost access.
    let lost = api::phone::revoke::lost_access_sessions(&pool, None, 10)
        .await
        .unwrap();
    assert_eq!(lost, [900_150]);
}

#[test_db::test(seed)]
async fn only_employees_and_admins_get_a_phone(pool: MySqlPool) {
    let (server, _) = server(pool.clone()).await;
    let response = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("viewer@acme.test", PASSWORD))
        .await;
    response.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Value>(),
        json!({ "error": "no_phone_access" })
    );
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM device_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    // A super admin (position 9) is an admin of their own company.
    let body: Value = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("super@other.test", PASSWORD))
        .await
        .json();
    assert_eq!(
        body["user"],
        json!({ "id": 103, "name": "Sam Super", "email": "super@other.test",
                "companyId": 101, "companyName": "Other Granite Co", "isAdmin": true })
    );
}

#[test_db::test(seed)]
async fn failed_sign_ins_are_limited_with_the_web_logins(pool: MySqlPool) {
    let (server, _) = server(pool.clone()).await;
    // Web and phone failures count together.
    for _ in 0..5 {
        server
            .post("/v1/auth/login")
            .json(&json!({ "email": "rep@acme.test", "password": "wrong-password" }))
            .await
            .assert_status_unauthorized();
        server
            .post("/v1/auth/device-sessions")
            .json(&sign_in_body("rep@acme.test", "wrong-password"))
            .await
            .assert_status_unauthorized();
    }
    // Ten failures from this address: even the right password waits.
    let response = server
        .post("/v1/auth/device-sessions")
        .json(&sign_in_body("rep@acme.test", PASSWORD))
        .await;
    response.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.json::<Value>(), json!({ "error": "rate_limited" }));
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD }))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    // Another address gets in, and success clears the email's failures.
    server
        .post("/v1/auth/device-sessions")
        .add_header("x-forwarded-for", "203.0.113.9")
        .json(&sign_in_body("rep@acme.test", PASSWORD))
        .await
        .assert_status(StatusCode::CREATED);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[test_db::test(seed)]
async fn my_devices_with_the_header_or_the_cookie(pool: MySqlPool) {
    // Rita: this phone, an old iPhone, and a revoked one; Mark's and Olga's
    // phones are theirs alone.
    add_session(&pool, 900101, 100, 100, 1, "device_name = 'Rita''s Pixel 9', app_version = '1.4.0', last_seen_at = UTC_TIMESTAMP() - INTERVAL 10 MINUTE").await;
    add_session(&pool, 900102, 100, 100, 2, "device_name = 'Rita''s old iPhone', platform = 'ios', app_version = NULL, created_at = '2026-09-01 08:00:00', last_seen_at = '2026-09-02 09:30:00'").await;
    add_session(
        &pool,
        900103,
        100,
        100,
        3,
        "revoked_at = UTC_TIMESTAMP(), revoked_reason = 'user_revoked'",
    )
    .await;
    add_session(&pool, 900104, 101, 100, 4, "").await;
    add_session(&pool, 900105, 106, 101, 5, "").await;
    let (server, _) = server(pool.clone()).await;

    let response = server
        .get("/v1/me/devices")
        .add_header(HEADER, token(1))
        .await;
    response.assert_status_ok();
    // This request counted as use (the last one was 10 minutes ago).
    let seen: DateTime<Utc> =
        sqlx::query_scalar("SELECT last_seen_at FROM device_sessions WHERE id = 900101")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!((Utc::now() - seen).num_seconds() < 60);
    assert_eq!(
        response.json::<Value>(),
        json!({ "devices": [
            { "id": 900101, "deviceName": "Rita's Pixel 9", "platform": "android", "appVersion": "1.4.0",
              "createdAt": "2026-10-01T12:00:00.000Z", "lastSeenAt": js(seen), "current": true },
            { "id": 900102, "deviceName": "Rita's old iPhone", "platform": "ios", "appVersion": null,
              "createdAt": "2026-09-01T08:00:00.000Z", "lastSeenAt": "2026-09-02T09:30:00.000Z",
              "current": false },
        ] })
    );
    // The CRM's cookie lists the same phones; none is the caller's.
    let web: Value = server
        .get("/v1/me/devices")
        .with_session(sessions::REP)
        .await
        .json();
    let ids: Vec<(i64, bool)> = web["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["id"].as_i64().unwrap(), d["current"].as_bool().unwrap()))
        .collect();
    assert_eq!(ids, [(900101, false), (900102, false)]);
    // Olga sees only hers.
    let olga: Value = server
        .get("/v1/me/devices")
        .add_header(HEADER, token(5))
        .await
        .json();
    assert_eq!(olga["devices"].as_array().unwrap().len(), 1);
    assert_eq!(olga["devices"][0]["id"], 900105);

    // No credentials, a malformed or unknown token: 401, even with a cookie.
    server
        .get("/v1/me/devices")
        .await
        .assert_status_unauthorized();
    server
        .get("/v1/me/devices")
        .add_header(HEADER, "nope")
        .await
        .assert_status_unauthorized();
    let unknown = server
        .get("/v1/me/devices")
        .add_header(HEADER, token(99))
        .with_session(sessions::REP)
        .await;
    unknown.assert_status_unauthorized();
    assert_eq!(unknown.json::<Value>(), json!({ "error": "unauthorized" }));
    // A revoked token is out.
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(3))
        .await
        .assert_status_unauthorized();
    // The cookie of a non-employee is refused like everywhere else.
    server
        .get("/v1/me/devices")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

/// Seconds since the device session was last seen.
async fn seconds_since_seen(pool: &MySqlPool, id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT CAST(TIMESTAMPDIFF(SECOND, last_seen_at, UTC_TIMESTAMP()) AS SIGNED)
           FROM device_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn last_seen_is_written_at_most_every_five_minutes(pool: MySqlPool) {
    add_session(
        &pool,
        900101,
        100,
        100,
        1,
        "last_seen_at = UTC_TIMESTAMP() - INTERVAL 2 MINUTE",
    )
    .await;
    let (server, _) = server(pool.clone()).await;
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(1))
        .await
        .assert_status_ok();
    assert!(
        seconds_since_seen(&pool, 900101).await >= 110,
        "a recent last_seen_at is kept"
    );
    sqlx::query("UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP() - INTERVAL 6 MINUTE WHERE id = 900101")
        .execute(&pool)
        .await
        .unwrap();
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(1))
        .await
        .assert_status_ok();
    assert!(
        seconds_since_seen(&pool, 900101).await < 60,
        "an older one is updated"
    );
}

#[test_db::test(seed)]
async fn idle_expired_and_removed_users_tokens_are_refused(pool: MySqlPool) {
    // Used 29 days ago: still in. 31 days: out, as is a session never used
    // since a sign-in 31 days ago, and one past its 12 months.
    add_session(
        &pool,
        900110,
        100,
        100,
        10,
        "last_seen_at = UTC_TIMESTAMP() - INTERVAL 29 DAY",
    )
    .await;
    add_session(
        &pool,
        900111,
        100,
        100,
        11,
        "last_seen_at = UTC_TIMESTAMP() - INTERVAL 31 DAY",
    )
    .await;
    add_session(
        &pool,
        900112,
        100,
        100,
        12,
        "created_at = UTC_TIMESTAMP() - INTERVAL 31 DAY, last_seen_at = NULL",
    )
    .await;
    add_session(
        &pool,
        900113,
        100,
        100,
        13,
        "expires_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE",
    )
    .await;
    // A deleted user, a user who is not an employee (any more), and a user
    // whose company is no longer the session's.
    add_session(&pool, 900114, 105, 100, 14, "").await;
    add_session(&pool, 900115, 104, 100, 15, "").await;
    add_session(&pool, 900116, 100, 101, 16, "").await;
    let (server, _) = server(pool).await;
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(10))
        .await
        .assert_status_ok();
    for n in 11..=16 {
        server
            .get("/v1/me/devices")
            .add_header(HEADER, token(n))
            .await
            .assert_status_unauthorized();
    }
}

#[test_db::test(seed)]
async fn a_phone_is_revoked_from_another_device_or_the_crm(pool: MySqlPool) {
    add_session(&pool, 900101, 100, 100, 1, "").await;
    add_session(&pool, 900102, 100, 100, 2, "").await;
    add_session(&pool, 900103, 100, 100, 3, "revoked_at = UTC_TIMESTAMP()").await;
    add_session(&pool, 900104, 101, 100, 4, "").await;
    add_session(&pool, 900105, 106, 101, 5, "").await;
    sqlx::query(
        "INSERT INTO phone_devices
           (id, device_session_id, user_id, company_id, platform, device_name,
            telnyx_credential_id, sip_username, push_token)
         VALUES (900202, 900102, 100, 100, 'android', 'Test phone', 'cred-900202', 'gencred900202', 'push-900202')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, deleted) = server(pool.clone()).await;
    // Another user's phone, another company's, a revoked one, a missing one: 404.
    for id in [900104, 900105, 900103, 999_999] {
        server
            .delete(&format!("/v1/me/devices/{id}"))
            .add_header(HEADER, token(1))
            .await
            .assert_status_not_found();
    }
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(4))
        .await
        .assert_status_ok();
    // Rita signs her other phone out from this one.
    server
        .delete("/v1/me/devices/900102")
        .add_header(HEADER, token(1))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(2))
        .await
        .assert_status_unauthorized();
    let (reason, device_reason, push, credential_deleted): (String, String, Option<String>, bool) =
        sqlx::query_as(
            "SELECT ds.revoked_reason, pd.revoke_reason, pd.push_token,
                    pd.credential_deleted_at IS NOT NULL
               FROM device_sessions ds JOIN phone_devices pd ON pd.device_session_id = ds.id
              WHERE ds.id = 900102",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (
            reason.as_str(),
            device_reason.as_str(),
            push,
            credential_deleted
        ),
        ("user_revoked", "user_revoked", None, true)
    );
    assert_eq!(*deleted.lock().unwrap(), ["cred-900202"]);
    // ... and this one from the CRM; then it is out too.
    server
        .delete("/v1/me/devices/900101")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(1))
        .await
        .assert_status_unauthorized();
    let live: Value = server
        .get("/v1/me/devices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(live, json!({ "devices": [] }));
}

/// What signing out left on a device session and its phone device: the
/// session's reason, the device revoked and its reason, a push token kept,
/// the credential and whether it is deleted.
async fn signed_out(
    pool: &MySqlPool,
    session_id: i64,
) -> (
    Option<String>,
    bool,
    Option<String>,
    bool,
    Option<String>,
    bool,
) {
    sqlx::query_as(
        "SELECT ds.revoked_reason, pd.revoked_at IS NOT NULL, pd.revoke_reason,
                pd.push_token IS NOT NULL, pd.telnyx_credential_id,
                pd.credential_deleted_at IS NOT NULL
           FROM device_sessions ds JOIN phone_devices pd ON pd.device_session_id = ds.id
          WHERE ds.id = ?",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn signing_out_revokes_the_phone_and_its_credential(pool: MySqlPool) {
    add_session(&pool, 900101, 100, 100, 1, "").await;
    add_session(&pool, 900106, 100, 100, 6, "").await;
    sqlx::query(
        "INSERT INTO phone_devices
           (id, device_session_id, user_id, company_id, platform, device_name,
            telnyx_credential_id, sip_username, push_token)
         VALUES (900201, 900101, 100, 100, 'android', 'Test phone', 'cred-900201', 'gencred900201', 'push-900201'),
                (900206, 900106, 100, 100, 'ios', 'Test phone', 'cred-fail', 'gencred900206', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, deleted) = server(pool.clone()).await;
    // The device token only: a cookie cannot sign a phone out.
    server
        .delete("/v1/auth/device-sessions/current")
        .with_session(sessions::REP)
        .await
        .assert_status_unauthorized();
    server
        .delete("/v1/auth/device-sessions/current")
        .add_header(HEADER, token(1))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        signed_out(&pool, 900101).await,
        (
            Some("signed_out".into()),
            true,
            Some("signed_out".into()),
            false,
            Some("cred-900201".into()),
            true
        )
    );
    assert_eq!(*deleted.lock().unwrap(), ["cred-900201"]);
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(1))
        .await
        .assert_status_unauthorized();
    server
        .delete("/v1/auth/device-sessions/current")
        .add_header(HEADER, token(1))
        .await
        .assert_status_unauthorized();
    // When Telnyx fails, the phone is still out and the credential waits
    // for the job.
    server
        .delete("/v1/auth/device-sessions/current")
        .add_header(HEADER, token(6))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        signed_out(&pool, 900106).await,
        (
            Some("signed_out".into()),
            true,
            Some("signed_out".into()),
            false,
            Some("cred-fail".into()),
            false
        )
    );
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(6))
        .await
        .assert_status_unauthorized();
}
