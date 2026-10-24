//! Granite Phone in the CRM: the browser as a phone (`routes/phone/web.rs`),
//! registered with the web session cookie. Its login JWTs (the app's
//! limits), presence (a connected browser rings with the app phones), its
//! calls (approved like the app's when signed with its call key), signing
//! it out (CRM sign-out, "Signed-in devices", the job), and cell
//! verification and Call via my cell with the cookie.
//!
//! Fixture phones use n = 1..=9; numbers 9871+.

mod common;
mod phone_common;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::{SECRET, WithSession, sessions};
use hmac::{Hmac, KeyInit, Mac};
use phone_common::{
    JOB_KEY, JWT, JWT_EXPIRES, add_number, add_phone, assign, enable_calling, hook_path,
    phone_server, post_texml, sql, webhook_base,
};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::MySqlPool;

const DEVICES: &str = "/v1/phone/web/devices";

/// Registers the browser of web session `session` ("Chrome on Windows").
async fn register(server: &TestServer, session: &str) -> TestResponse {
    server
        .post(DEVICES)
        .with_session(session)
        .json(&json!({ "deviceName": " Chrome on Windows " }))
        .await
}

/// The call key of device session `session_id`, made apart from the API:
/// base64url of HMAC-SHA256 keyed with `SESSION_SECRET`.
fn call_key_of(session_id: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("granite-phone-call-key:{session_id}").as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// `X-Granite-Call-Auth` as the dock makes it with the `callKey` it got.
fn sign(call_key: &str, device: &str, to: &str, caller: &str, time: i64) -> String {
    let key = URL_SAFE_NO_PAD.decode(call_key).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
    mac.update(format!("v1|{device}|{to}|{caller}|{time}").as_bytes());
    format!(
        "v1.{time}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

/// A browser phone's row: (platform, web session, user, company, device
/// name, revoked).
async fn device_row(pool: &MySqlPool, id: i64) -> (String, Option<String>, i32, i32, String, bool) {
    sqlx::query_as(
        "SELECT CAST(pd.platform AS CHAR), ds.web_session_id, pd.user_id, pd.company_id,
                ds.device_name, (pd.revoked_at IS NOT NULL AND ds.revoked_at IS NOT NULL)
           FROM phone_devices pd JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE pd.id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Telnyx's voice-URL request for call `sid` to Rita's line.
async fn ring_rita(server: &TestServer, sid: &str) -> String {
    let response = post_texml(
        server,
        100,
        "inbound",
        &[
            ("CallSid", sid),
            ("ConnectionId", "app-100"),
            ("From", "+13175550142"),
            ("To", "+13175550171"),
            ("CallStatus", "ringing"),
        ],
    )
    .await;
    response.assert_status_ok();
    response.text()
}

/// Rita's direct line, hers to ring and call from.
async fn rita_line(pool: &MySqlPool) {
    add_number(pool, 9871, 100, "+13175550171", "direct", "Rita direct").await;
    assign(pool, 9871, 100, true, true).await;
}

async fn presence(server: &TestServer, session: &str, id: i64, online: bool) -> TestResponse {
    server
        .put(&format!("{DEVICES}/{id}/presence"))
        .with_session(session)
        .json(&json!({ "online": online }))
        .await
}

#[test_db::test(seed)]
async fn a_browser_registers_once_per_web_session_with_the_cookie(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, stubs) = phone_server(pool.clone()).await;

    let first = register(&server, sessions::REP).await;
    first.assert_status(StatusCode::CREATED);
    assert_eq!(first.header("cache-control"), "no-store");
    let body: Value = first.json();
    let id = body["id"].as_i64().unwrap();
    let session_id = body["sessionId"].as_i64().unwrap();
    assert_eq!(body["sipUsername"], "gencrednew1");
    assert_eq!(body["callKey"], call_key_of(session_id));
    assert!(body["serverTime"].as_str().unwrap().ends_with('Z'));
    // Its own credential on the company's connection, named by ids only.
    assert_eq!(stubs.telnyx_paths(), ["POST /telephony_credentials"]);
    let request = &stubs.telnyx()[0].2;
    assert_eq!(request["name"], format!("phone-{id}"));
    assert_eq!(request["connection_id"], "conn-100");
    assert_eq!(
        device_row(&pool, id).await,
        (
            "web".into(),
            Some(sessions::REP.into()),
            100,
            100,
            "Chrome on Windows".into(),
            false
        )
    );

    // Every tab of the session gets the same phone and key: no second
    // credential.
    let again = register(&server, sessions::REP).await;
    again.assert_status_ok();
    let again: Value = again.json();
    assert_eq!(
        (&again["id"], &again["sipUsername"], &again["callKey"]),
        (
            &json!(id),
            &json!("gencrednew1"),
            &json!(call_key_of(session_id))
        )
    );
    assert_eq!(stubs.telnyx().len(), 1);

    // Another web session (another browser) is another phone.
    let other: Value = register(&server, sessions::MANAGER).await.json();
    assert_ne!(other["id"], json!(id));
    assert_eq!(other["sipUsername"], "gencrednew2");

    // "Signed-in devices" lists it as a browser.
    let listed: Value = server
        .get("/v1/me/devices")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(listed["devices"][0]["id"], json!(session_id));
    assert_eq!(listed["devices"][0]["platform"], "web");
    assert_eq!(listed["devices"][0]["deviceName"], "Chrome on Windows");
    let admin: Value = server
        .get("/v1/phone/admin/devices")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert!(
        admin["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["platform"] == "web" && device["sipUsername"] == "gencrednew1")
    );
    // The app's device-token routes take no cookie.
    server
        .post(&format!("/v1/phone/devices/{id}/token"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn browsers_need_calling_on_and_an_employee_cookie(pool: MySqlPool) {
    let (server, stubs) = phone_server(pool.clone()).await;
    // No Telnyx setup for the company: calling is off.
    let off = register(&server, sessions::REP).await;
    off.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(off.json::<Value>(), json!({ "error": "calling_disabled" }));
    enable_calling(&pool, 100, "enabled = 0").await;
    register(&server, sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    sql(
        &pool,
        "UPDATE telnyx_company_config SET enabled = 1 WHERE company_id = 100",
    )
    .await;
    register(&server, sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post(DEVICES)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    register(&server, sessions::LOGGED_OUT)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    assert!(stubs.telnyx().is_empty());
    // The bootstrap says calling is on once it is.
    let bootstrap: Value = server
        .get("/v1/phone/web/bootstrap")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(bootstrap["callingEnabled"], true);
    assert_eq!(bootstrap["recordingMode"], "inbound");
    assert_eq!(bootstrap["callerIds"], json!([]));
    assert_eq!(bootstrap["defaultNumberId"], Value::Null);
    assert_eq!(bootstrap["settings"]["cellVerified"], false);
    assert_eq!(bootstrap["availability"]["status"], "available");
}

#[test_db::test(seed)]
async fn the_browser_gets_jwts_with_the_cookie_under_the_apps_limits(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let id = register(&server, sessions::REP).await.json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let token = |session: &'static str, device: i64| {
        server
            .post(&format!("{DEVICES}/{device}/token"))
            .with_session(session)
    };

    let first = token(sessions::REP, id).await;
    first.assert_status_ok();
    assert_eq!(first.header("cache-control"), "no-store");
    assert_eq!(
        first.json::<Value>(),
        json!({ "jwt": JWT, "expiresAt": JWT_EXPIRES })
    );
    assert_eq!(
        stubs.telnyx_paths()[1..],
        [
            "PATCH /telephony_credentials/cred-new-1",
            "POST /telephony_credentials/cred-new-1/token"
        ]
    );
    // One a minute...
    let soon = token(sessions::REP, id).await;
    soon.assert_status(StatusCode::TOO_MANY_REQUESTS);
    let wait = soon.json::<Value>()["retryAfterSecs"].as_u64().unwrap();
    assert!((1..=60).contains(&wait));
    // ... six an hour.
    sql(
        &pool,
        &format!(
            "UPDATE phone_devices SET last_token_at = UTC_TIMESTAMP() - INTERVAL 2 MINUTE,
                    token_window_start = UTC_TIMESTAMP() - INTERVAL 10 MINUTE,
                    token_window_count = 6 WHERE id = {id}"
        ),
    )
    .await;
    let hourly = token(sessions::REP, id).await;
    hourly.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert!(hourly.json::<Value>()["retryAfterSecs"].as_u64().unwrap() > 2900);
    // Another web session's browser, or no phone at all.
    token(sessions::MANAGER, id)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    token(sessions::REP, id + 1000)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn signing_out_signs_the_browser_phone_out(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    let (server, stubs) = phone_server(pool.clone()).await;

    // Signing out of the CRM: the browser phone and its credential go.
    let rita = register(&server, sessions::REP).await.json::<Value>()["id"]
        .as_i64()
        .unwrap();
    stubs.clear_telnyx();
    server
        .post("/v1/auth/logout")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(device_row(&pool, rita).await.5);
    assert_eq!(
        stubs.telnyx_paths(),
        ["DELETE /telephony_credentials/cred-new-1"]
    );

    // "Signed-in devices": the dock hears `device_revoked` and turns off;
    // turning it on again registers a new phone.
    let mark: Value = register(&server, sessions::MANAGER).await.json();
    let (mark_id, mark_session) = (
        mark["id"].as_i64().unwrap(),
        mark["sessionId"].as_i64().unwrap(),
    );
    server
        .delete(&format!("/v1/me/devices/{mark_session}"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let revoked = presence(&server, sessions::MANAGER, mark_id, true).await;
    revoked.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(
        revoked.json::<Value>(),
        json!({ "error": "device_revoked" })
    );
    let renewed = register(&server, sessions::MANAGER).await;
    renewed.assert_status(StatusCode::CREATED);
    assert_ne!(renewed.json::<Value>()["id"], json!(mark_id));

    // A web session that ended another way: the job signs its browser
    // phone out.
    let ada = register(&server, sessions::ADMIN).await.json::<Value>()["id"]
        .as_i64()
        .unwrap();
    sql(
        &pool,
        &format!(
            "UPDATE sessions SET is_deleted = 1 WHERE id = '{}'",
            sessions::ADMIN
        ),
    )
    .await;
    server
        .post("/v1/jobs/phone")
        .add_header("authorization", JOB_KEY)
        .await
        .assert_status_ok();
    assert!(device_row(&pool, ada).await.5);
}

#[test_db::test(seed)]
async fn a_connected_browser_rings_with_the_apps_phones(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'off'").await;
    rita_line(&pool).await;
    add_phone(&pool, 1, 100, 100).await;
    let (server, _stubs) = phone_server(pool.clone()).await;
    let browser = register(&server, sessions::REP).await.json::<Value>()["id"]
        .as_i64()
        .unwrap();
    let app_sip = "sip:gencred1@sip.telnyx.com";
    let browser_sip = "sip:gencrednew1@sip.telnyx.com";

    // Registered but not connected: only the app rings.
    let texml = ring_rita(&server, "v3:in-1").await;
    assert!(
        texml.contains(app_sip) && !texml.contains(browser_sip),
        "{texml}"
    );

    // Connected: both ring, each with its own leg callback.
    presence(&server, sessions::REP, browser, true)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let texml = ring_rita(&server, "v3:in-2").await;
    assert!(texml.contains(app_sip), "{texml}");
    assert!(
        texml.contains(&format!("device={browser}\"")) && texml.contains(browser_sip),
        "{texml}"
    );
    let legs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM phone_call_legs l JOIN phone_calls pc ON pc.id = l.call_id
          WHERE pc.telnyx_call_sid = 'v3:in-2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(legs, 2);

    // Declining in the browser: nobody else's phone rings, so the caller
    // goes on to the no-answer action (which stops the app too).
    let declined = server
        .post(&format!("{DEVICES}/{browser}/decline"))
        .with_session(sessions::REP)
        .await;
    declined.assert_status(StatusCode::NO_CONTENT);
    let leg_status: String = sqlx::query_scalar(
        "SELECT l.status FROM phone_call_legs l JOIN phone_calls pc ON pc.id = l.call_id
          WHERE pc.telnyx_call_sid = 'v3:in-2' AND l.device_id = ?",
    )
    .bind(browser)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(leg_status, "declined");

    // Offline again (the dock closed): only the app.
    presence(&server, sessions::REP, browser, false)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let texml = ring_rita(&server, "v3:in-3").await;
    assert!(!texml.contains(browser_sip), "{texml}");

    // Connected, but its web session ended: it does not ring.
    presence(&server, sessions::REP, browser, true).await;
    sql(
        &pool,
        &format!(
            "UPDATE sessions SET is_deleted = 1 WHERE id = '{}'",
            sessions::REP
        ),
    )
    .await;
    let texml = ring_rita(&server, "v3:in-4").await;
    assert!(
        !texml.contains(browser_sip) && texml.contains(app_sip),
        "{texml}"
    );
}

#[test_db::test(seed)]
async fn declining_redirects_the_caller_when_nobody_else_rings(pool: MySqlPool) {
    enable_calling(&pool, 100, "recording_mode = 'off'").await;
    rita_line(&pool).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let browser = register(&server, sessions::REP).await.json::<Value>()["id"]
        .as_i64()
        .unwrap();
    presence(&server, sessions::REP, browser, true).await;
    ring_rita(&server, "v3:in-9").await;
    let call: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM phone_calls WHERE telnyx_call_sid = 'v3:in-9'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    stubs.clear_telnyx();
    server
        .post(&format!("{DEVICES}/{browser}/decline"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/texml/calls/v3:in-9/update".to_string(),
            json!({ "Url": format!("{}/dial-result?call={call}&declined=1", webhook_base(100)),
                    "Method": "POST" })
        )]
    );
    // Nothing rings it now: nothing to do.
    stubs.clear_telnyx();
    server
        .post(&format!("{DEVICES}/{browser}/decline"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(stubs.telnyx().is_empty());
    // Not this session's browser.
    server
        .post(&format!("{DEVICES}/{browser}/decline"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// Posts a Call Control event as Telnyx does (unsigned: the phone tests'
/// server takes those).
async fn call_control(server: &TestServer, body: &Value) {
    server
        .post(&hook_path(100, "outbound"))
        .content_type("application/json")
        .bytes(Bytes::from(serde_json::to_vec(body).unwrap()))
        .await
        .assert_status_ok();
}

/// The browser's parked call `ccid` to `to`, with these SIP headers.
fn parked(id: &str, ccid: &str, to: &str, headers: &[(&str, &str)]) -> Value {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    json!({ "data": {
        "record_type": "event", "id": id, "event_type": "call.initiated",
        "occurred_at": chrono::Utc::now().to_rfc3339(),
        "payload": {
            "call_control_id": ccid, "call_session_id": format!("session-{ccid}"),
            "connection_id": "conn-100", "from": "+13175550171", "to": to,
            "direction": "outgoing", "state": "parked", "custom_headers": headers } } })
}

#[test_db::test(seed)]
async fn a_browser_call_signed_with_its_call_key_is_approved(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita_line(&pool).await;
    let (server, stubs) = phone_server(pool.clone()).await;
    let phone: Value = register(&server, sessions::REP).await.json();
    let device = phone["id"].as_i64().unwrap().to_string();
    let key = phone["callKey"].as_str().unwrap();
    let now = chrono::Utc::now().timestamp();
    let to = "+13175550142";
    let caller = "+13175550171";

    stubs.clear_telnyx();
    let good = sign(key, &device, to, caller, now);
    call_control(
        &server,
        &parked(
            "ev-1",
            "v3:web-1",
            to,
            &[
                ("X-Granite-Device-Id", &device),
                ("X-Granite-Caller-Id", caller),
                ("X-Granite-Call-Auth", &good),
                ("X-Granite-Customer-Id", "1000"),
            ],
        ),
    )
    .await;
    let (id, user, status, customer): (i64, Option<i32>, String, Option<i32>) = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), user_id, CAST(status AS CHAR), customer_id FROM phone_calls
          WHERE telnyx_call_control_id = 'v3:web-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (user, status.as_str(), customer),
        (Some(100), "ringing", Some(1000))
    );
    assert_eq!(
        stubs.telnyx(),
        [(
            "POST".to_string(),
            "/calls/v3:web-1/actions/transfer".to_string(),
            json!({ "to": to, "from": caller, "timeout_secs": 60,
                    "webhook_url": format!("{}/outbound", webhook_base(100)),
                    "command_id": format!("approve-{id}") })
        )]
    );

    // Signed for another number, with another key, or not at all: hung up.
    let other_key = call_key_of(999_999);
    let refused = [
        (
            "ev-2",
            "v3:web-2",
            sign(key, &device, "+13175550143", caller, now),
        ),
        (
            "ev-3",
            "v3:web-3",
            sign(&other_key, &device, to, caller, now),
        ),
        (
            "ev-4",
            "v3:web-4",
            sign(key, &device, to, caller, now - 3600),
        ),
    ];
    for (event, ccid, auth) in refused {
        stubs.clear_telnyx();
        call_control(
            &server,
            &parked(
                event,
                ccid,
                to,
                &[
                    ("X-Granite-Device-Id", &device),
                    ("X-Granite-Caller-Id", caller),
                    ("X-Granite-Call-Auth", &auth),
                ],
            ),
        )
        .await;
        assert_eq!(
            stubs.telnyx_paths(),
            [format!("POST /calls/{ccid}/actions/hangup")],
            "{ccid}"
        );
    }
    stubs.clear_telnyx();
    call_control(
        &server,
        &parked(
            "ev-5",
            "v3:web-5",
            to,
            &[
                ("X-Granite-Device-Id", &device),
                ("X-Granite-Caller-Id", caller),
            ],
        ),
    )
    .await;
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /calls/v3:web-5/actions/hangup"]
    );

    // Signed out of the CRM: the browser's calls are refused too.
    server
        .post("/v1/auth/logout")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    stubs.clear_telnyx();
    let late = sign(key, &device, to, caller, chrono::Utc::now().timestamp());
    call_control(
        &server,
        &parked(
            "ev-6",
            "v3:web-6",
            to,
            &[
                ("X-Granite-Device-Id", &device),
                ("X-Granite-Caller-Id", caller),
                ("X-Granite-Call-Auth", &late),
            ],
        ),
    )
    .await;
    assert_eq!(
        stubs.telnyx_paths(),
        ["POST /calls/v3:web-6/actions/hangup"]
    );
}

#[test_db::test(seed)]
async fn the_crm_verifies_the_cell_and_calls_via_it_with_the_cookie(pool: MySqlPool) {
    enable_calling(&pool, 100, "").await;
    rita_line(&pool).await;
    let (server, stubs) = phone_server(pool.clone()).await;

    let verify = server
        .post("/v1/phone/cell/verify")
        .with_session(sessions::REP)
        .json(&json!({ "cellNumber": "317-555-0177" }))
        .await;
    verify.assert_status(StatusCode::ACCEPTED);
    let sent = stubs.telnyx();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, "/texml/calls/app-100");
    assert_eq!(
        (&sent[0].2["To"], &sent[0].2["From"]),
        (&json!("+13175550177"), &json!("+13175550171"))
    );
    let code: String =
        sqlx::query_scalar("SELECT cell_code FROM phone_user_settings WHERE user_id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    let confirmed = server
        .post("/v1/phone/cell/confirm")
        .with_session(sessions::REP)
        .json(&json!({ "code": code }))
        .await;
    confirmed.assert_status_ok();
    let settings: Value = confirmed.json();
    assert_eq!(
        (&settings["cellNumber"], &settings["cellVerified"]),
        (&json!("+13175550177"), &json!(true))
    );

    // Call via my cell from the CRM.
    stubs.clear_telnyx();
    let call = server
        .post("/v1/phone/calls/via-cell")
        .with_session(sessions::REP)
        .json(&json!({ "to": "317-555-0142", "callerNumberId": null, "customerId": 1000 }))
        .await;
    call.assert_status(StatusCode::ACCEPTED);
    let sent = stubs.telnyx();
    assert_eq!(sent[0].2["To"], "+13175550177");

    // Not for a user who is no employee.
    server
        .post("/v1/phone/cell/verify")
        .with_session(sessions::VIEWER)
        .json(&json!({ "cellNumber": "317-555-0177" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/phone/cell/confirm")
        .json(&json!({ "code": "123456" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
