//! A customer's own page: unlocked by an emailed one-time code (by page or
//! by the account's email), then their projects and invoices.

use crate::common;

use api::app;
use api::auth::cookie::{cookie_value, decode_session_map, encode_plain, encode_session_map};
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const VIEW: &str = "3b1f6c2e-9d4a-4e8b-a1c7-5f2e8d9c0b1a";
const FIXTURE: &str = "
UPDATE customers SET view_id = UUID_TO_BIN('3b1f6c2e-9d4a-4e8b-a1c7-5f2e8d9c0b1a'), email_id = 5001 WHERE id = 1000;
";

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn send_email(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    sent.lock().unwrap().push(email);
    Json(json!({ "message_id": "<c@ses.test>" }))
}

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let sent = Sent::default();
    let router = Router::new()
        .route("/email/send", post(send_email))
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    let mut server = TestServer::new(app(state));
    server.save_cookies();
    (server, sent)
}

/// The code in the last email.
fn last_code(sent: &Sent) -> String {
    let body = sent.lock().unwrap().last().unwrap().body.clone();
    let start = body.find("margin: 24px 0;\">").unwrap() + "margin: 24px 0;\">".len();
    body[start..start + 6].to_string()
}

fn page(path: &str) -> String {
    format!("/v1/public/customer-view/100/{VIEW}{path}")
}

#[test_db::test(seed)]
async fn a_customer_unlocks_their_page_with_an_emailed_code(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let gate = server.get(&page("")).await;
    gate.assert_status_ok();
    assert_eq!(
        gate.json::<Value>(),
        json!({ "verified": false, "customer_name": null, "has_email": true,
                "masked_email": "a***@example.com", "code_sent": false })
    );
    server
        .get(&page("/projects"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let email = sent.lock().unwrap()[0].clone();
    assert_eq!(email.to, vec!["alice@example.com".to_string()]);
    assert_eq!(email.subject, "Your customer login code");
    assert_eq!(
        server.get(&page("")).await.json::<Value>()["code_sent"],
        true
    );

    // Another code right away is refused.
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);

    let res = server
        .post(&page("/verify"))
        .json(&json!({ "code": "000000x" }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "That code is not valid.");

    server
        .post(&page("/verify"))
        .json(&json!({ "code": last_code(&sent) }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let gate = server.get(&page("")).await.json::<Value>();
    assert_eq!(gate["verified"], true);
    assert_eq!(gate["customer_name"], "Alice Lead");

    let projects = server.get(&page("/projects")).await;
    projects.assert_status_ok();
    assert_eq!(projects.header("cache-control"), "private, no-store");
    let projects = projects.json::<Value>();
    assert_eq!(projects[0]["id"], 7010);
    assert_eq!(projects[0]["status"], "Sold");
    assert_eq!(projects[0]["seller_name"], "Rita Rep");
    assert_eq!(projects[0]["price"], 4200.0);

    let invoices = server.get(&page("/invoices")).await;
    invoices.assert_status_ok();
    assert_eq!(invoices.json::<Value>()["status"], "not_connected");

    // The page of another customer stays locked.
    sqlx::raw_sql(
        "UPDATE customers SET view_id = UUID_TO_BIN('4c2a7d3f-0e5b-4f9c-b2d8-6a3f9e0d1c2b') WHERE id = 1004",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .get("/v1/public/customer-view/100/4c2a7d3f-0e5b-4f9c-b2d8-6a3f9e0d1c2b/projects")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

#[test_db::test(seed)]
async fn unknown_pages_are_not_found(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    server
        .get(&format!("/v1/public/customer-view/101/{VIEW}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/public/customer-view/100/not-a-uuid")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_customer_signs_in_with_their_email(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let login = "/v1/public/customer-view/100/login";
    assert_eq!(
        server.get(login).await.json::<Value>(),
        json!({ "code_sent": false, "masked_email": null })
    );
    server
        .post(&format!("{login}/code"))
        .json(&json!({ "email": "not an email" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Any customer address of the company works.
    server
        .post(&format!("{login}/code"))
        .json(&json!({ "email": "Alice.Work@Example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        server.get(login).await.json::<Value>(),
        json!({ "code_sent": true, "masked_email": "a***@example.com" })
    );
    let res = server
        .post(&format!("{login}/verify"))
        .json(&json!({ "code": last_code(&sent) }))
        .await;
    res.assert_status_ok();
    assert_eq!(res.json::<Value>(), json!({ "view_id": VIEW }));
    assert_eq!(
        server.get(&page("")).await.json::<Value>()["verified"],
        true
    );
}

#[test_db::test(seed)]
async fn an_unknown_email_looks_the_same_but_never_verifies(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let login = "/v1/public/customer-view/100/login";
    server
        .post(&format!("{login}/code"))
        .json(&json!({ "email": "stranger@example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(sent.lock().unwrap().is_empty());
    assert_eq!(
        server.get(login).await.json::<Value>(),
        json!({ "code_sent": true, "masked_email": "s***@example.com" })
    );
    server
        .post(&format!("{login}/verify"))
        .json(&json!({ "code": "123456" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Cancelling forgets it.
    server
        .delete("/v1/public/customer-view/login")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(server.get(login).await.json::<Value>()["code_sent"], false);
}

/// The `Set-Cookie` value of a response, as a `Cookie` header.
fn set_cookie(res: &axum_test::TestResponse) -> String {
    res.header("set-cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// Attempts are counted on the server: replaying the cookie a code was sent
/// with does not give more guesses, and the cookie names neither the page
/// nor the address.
#[test_db::test(seed)]
async fn replaying_the_cookie_does_not_reset_the_attempts(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post("/v1/public/customer-view/100/login/code")
        .json(&json!({ "email": "alice@example.com" }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let cookie = set_cookie(&res);
    let payload = percent_encoding::percent_decode_str(cookie.split_once('=').unwrap().1)
        .decode_utf8()
        .unwrap();
    let json = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        payload.split('.').next().unwrap(),
    )
    .unwrap();
    let json = String::from_utf8(json).unwrap();
    assert!(!json.contains(VIEW) && !json.contains("alice"), "{json}");

    let code = last_code(&sent);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..8 {
        let res = server
            .post("/v1/public/customer-view/100/login/verify")
            .add_header("cookie", &cookie)
            .json(&json!({ "code": wrong }))
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], "That code is not valid.");
    }
    let res = server
        .post("/v1/public/customer-view/100/login/verify")
        .add_header("cookie", &cookie)
        .json(&json!({ "code": code }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Too many attempts. Request a new code."
    );
}

/// An address gets at most 5 codes an hour, real or not, and each new code
/// replaces the one before.
#[test_db::test(seed)]
async fn codes_are_limited_per_address(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let earlier = |pool: MySqlPool| async move {
        sqlx::query("UPDATE customer_view_codes SET created_at = created_at - INTERVAL 1 MINUTE")
            .execute(&pool)
            .await
            .unwrap();
    };
    let send = |email: &'static str| {
        server
            .post("/v1/public/customer-view/100/login/code")
            .json(&json!({ "email": email }))
    };
    let first = send("alice@example.com").await;
    first.assert_status(StatusCode::NO_CONTENT);
    let first_code = last_code(&sent);
    send("alice@example.com")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    for _ in 0..4 {
        earlier(pool.clone()).await;
        send("alice@example.com")
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    earlier(pool.clone()).await;
    let res = send("alice@example.com").await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        res.json::<Value>()["error"],
        "Too many codes were requested. Please try again later."
    );
    assert_eq!(sent.lock().unwrap().len(), 5);

    // The first code was replaced.
    let res = server
        .post("/v1/public/customer-view/100/login/verify")
        .add_header("cookie", &set_cookie(&first))
        .json(&json!({ "code": first_code }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "That code expired. Request a new one."
    );

    // Unknown addresses are limited the same way.
    for _ in 0..5 {
        send("stranger@example.com")
            .await
            .assert_status(StatusCode::NO_CONTENT);
        earlier(pool.clone()).await;
    }
    send("stranger@example.com")
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
}

/// A code sent for one page does not unlock another customer's page.
#[test_db::test(seed)]
async fn a_code_unlocks_only_its_own_page(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    sqlx::raw_sql(
        "UPDATE customers SET view_id = UUID_TO_BIN('4c2a7d3f-0e5b-4f9c-b2d8-6a3f9e0d1c2b') WHERE id = 1004",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let other = "/v1/public/customer-view/100/4c2a7d3f-0e5b-4f9c-b2d8-6a3f9e0d1c2b";
    assert_eq!(server.get(other).await.json::<Value>()["code_sent"], false);
    server
        .post(&format!("{other}/verify"))
        .json(&json!({ "code": last_code(&sent) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get(&format!("{other}/projects"))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

// ---- Server-side codes: the attacks the cookie counter allowed, the limits,
// and the behaviour that must not change.

const LOGIN: &str = "/v1/public/customer-view/100/login";
const COOKIE_NAME: &str = "__customer_view";

/// A server whose email stand-in fails (500) while `fail` is set.
async fn flaky_server(pool: &MySqlPool) -> (TestServer, Sent, Arc<AtomicBool>) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let sent = Sent::default();
    let fail = Arc::new(AtomicBool::new(false));
    let (record, failing) = (sent.clone(), fail.clone());
    let router = Router::new().route(
        "/email/send",
        post(move |Json(email): Json<OutgoingEmail>| {
            let (record, failing) = (record.clone(), failing.clone());
            async move {
                if failing.load(Ordering::SeqCst) {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({}))).into_response();
                }
                record.lock().unwrap().push(email);
                Json(json!({ "message_id": "<c@ses.test>" })).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")));
    (TestServer::new(app(state)), sent, fail)
}

/// A `Cookie` header carrying `data` signed with `secret`.
fn signed(data: &Value, secret: &str) -> String {
    format!(
        "{COOKIE_NAME}={}",
        encode_session_map(data.as_object().unwrap(), secret)
    )
}

/// The JSON a `Cookie` header carries (signature checked).
fn cookie_json(cookie: &str) -> Value {
    let value = cookie_value(cookie, COOKIE_NAME).unwrap();
    Value::Object(decode_session_map(&value, SECRET).unwrap())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Every code row's attempts, oldest first.
async fn attempts(pool: &MySqlPool) -> Vec<i32> {
    sqlx::query_scalar("SELECT attempts FROM customer_view_codes ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Moves every code back a minute, past the 45 s cooldown.
async fn after_cooldown(pool: &MySqlPool) {
    sqlx::query("UPDATE customer_view_codes SET created_at = created_at - INTERVAL 1 MINUTE")
        .execute(pool)
        .await
        .unwrap();
}

async fn verify(server: &TestServer, path: &str, cookie: &str, code: &str) -> (StatusCode, Value) {
    let res = server
        .post(path)
        .add_header("cookie", cookie)
        .json(&json!({ "code": code }))
        .await;
    let status = res.status_code();
    let body = if status == StatusCode::NO_CONTENT {
        Value::Null
    } else {
        res.json::<Value>()
    };
    (status, body)
}

fn wrong(code: &str) -> &'static str {
    if code == "000000" { "111111" } else { "000000" }
}

/// The page route counts attempts on the server too: replaying the cookie
/// a code was sent with gives no more than 8 guesses.
#[test_db::test(seed)]
async fn replaying_the_cookie_on_the_page_route_does_not_reset_the_attempts(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server.post(&page("/code")).await;
    res.assert_status(StatusCode::NO_CONTENT);
    let cookie = set_cookie(&res);
    let code = last_code(&sent);
    for _ in 0..8 {
        let (status, body) = verify(&server, &page("/verify"), &cookie, wrong(&code)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "That code is not valid.");
    }
    let (status, body) = verify(&server, &page("/verify"), &cookie, &code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "Too many attempts. Request a new code.");
    assert_eq!(attempts(&pool).await, [8]);
    server
        .get(&page("/projects"))
        .add_header("cookie", &cookie)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// Guesses sent all at once with the same cookie are counted one by one in
/// the database: exactly 8 are checked, the rest refused, and the right code
/// afterwards is refused too.
#[test_db::test(seed)]
async fn concurrent_guesses_are_counted_exactly(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await;
    let cookie = set_cookie(&res);
    let code = last_code(&sent);
    let path = format!("{LOGIN}/verify");
    let guesses = (0..20).map(|_| verify(&server, &path, &cookie, wrong(&code)));
    let answers = futures_util::future::join_all(guesses).await;
    let invalid = answers
        .iter()
        .filter(|(_, body)| body["error"] == "That code is not valid.")
        .count();
    let refused = answers
        .iter()
        .filter(|(_, body)| body["error"] == "Too many attempts. Request a new code.")
        .count();
    assert!(
        answers
            .iter()
            .all(|(status, _)| *status == StatusCode::BAD_REQUEST)
    );
    assert_eq!((invalid, refused), (8, 12));
    assert_eq!(attempts(&pool).await, [8]);
    let (_, body) = verify(&server, &format!("{LOGIN}/verify"), &cookie, &code).await;
    assert_eq!(body["error"], "Too many attempts. Request a new code.");
}

/// A code works once: replaying the cookie it was sent with after it
/// verified finds nothing.
#[test_db::test(seed)]
async fn a_verified_code_cannot_be_used_again(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server.post(&page("/code")).await;
    let cookie = set_cookie(&res);
    let code = last_code(&sent);
    let (status, _) = verify(&server, &page("/verify"), &cookie, &code).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customer_view_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    for path in [page("/verify"), format!("{LOGIN}/verify")] {
        let (status, body) = verify(&server, &path, &cookie, &code).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "That code is not valid.");
    }
}

/// A code is the company's: a cookie of company 100 verifies nothing on
/// company 101 (and spends none of the code's attempts there).
#[test_db::test(seed)]
async fn a_code_is_only_good_at_its_company(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await;
    let cookie = set_cookie(&res);
    let code = last_code(&sent);
    for _ in 0..10 {
        let (status, body) = verify(
            &server,
            "/v1/public/customer-view/101/login/verify",
            &cookie,
            &code,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "That code is not valid.");
    }
    let other = server
        .get("/v1/public/customer-view/101/login")
        .add_header("cookie", &cookie)
        .await
        .json::<Value>();
    assert_eq!(other, json!({ "code_sent": false, "masked_email": null }));
    assert_eq!(attempts(&pool).await, [0]);
    let (status, body) = verify(&server, &format!("{LOGIN}/verify"), &cookie, &code).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "view_id": VIEW }));
}

/// Only a cookie signed with the server's secret holds a code: a tampered
/// payload, a foreign signature or an unsigned cookie naming a real code
/// verify nothing.
#[test_db::test(seed)]
async fn forged_cookies_hold_no_code(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await;
    let real = set_cookie(&res);
    let code = last_code(&sent);
    let token: String = sqlx::query_scalar("SELECT token FROM customer_view_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    let data = json!({ "pendingCode": token });
    let (name, value) = real.split_once('=').unwrap();
    let tampered = {
        let (payload, signature) = value.rsplit_once('.').unwrap();
        let flipped = if signature.starts_with('A') { "B" } else { "A" };
        format!("{name}={payload}.{flipped}{}", &signature[1..])
    };
    for cookie in [
        signed(&data, "not-the-secret"),
        format!("{COOKIE_NAME}={}", encode_plain(&data)),
        tampered,
        format!("{COOKIE_NAME}=garbage"),
    ] {
        let (status, body) = verify(&server, &format!("{LOGIN}/verify"), &cookie, &code).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{cookie}");
        assert_eq!(body["error"], "That code is not valid.");
        let (status, _) = verify(&server, &page("/verify"), &cookie, &code).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{cookie}");
    }
    assert_eq!(attempts(&pool).await, [0]);
}

/// Cookies from before codes moved to the database (pending code hash,
/// attempts and expiry in the cookie itself) no longer verify anything,
/// even when signed and holding the right hash.
#[test_db::test(seed)]
async fn legacy_pending_cookies_no_longer_verify(pool: MySqlPool) {
    let (mut server, _) = server(&pool).await;
    server.do_not_save_cookies();
    let code = "123456";
    let hash: String = Sha256::digest(format!("{SECRET}:{VIEW}:{code}").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let legacy = signed(
        &json!({
            "pendingViewId": VIEW,
            "pendingCompanyId": 100,
            "pendingEmail": "alice@example.com",
            "pendingCodeHash": hash,
            "pendingExpiresAt": now_ms() + 600_000,
            "pendingAttempts": 0,
            "pendingLastSentAt": now_ms(),
        }),
        SECRET,
    );
    for path in [page("/verify"), format!("{LOGIN}/verify")] {
        let (status, body) = verify(&server, &path, &legacy, code).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "That code is not valid.");
    }
    let gate = server
        .get(&page(""))
        .add_header("cookie", &legacy)
        .await
        .json::<Value>();
    assert_eq!(gate["code_sent"], false);
    assert_eq!(gate["verified"], false);
    assert_eq!(
        server
            .get(LOGIN)
            .add_header("cookie", &legacy)
            .await
            .json::<Value>()["code_sent"],
        false
    );
    // The next code clears the old fields from the cookie.
    let res = server
        .post(&page("/code"))
        .add_header("cookie", &legacy)
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let keys: Vec<String> = cookie_json(&set_cookie(&res))
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys, ["pendingCode"]);
}

/// A pretended code for an unknown address never verifies, whatever is
/// typed, and its attempts are counted like a real one's.
#[test_db::test(seed)]
async fn a_code_for_an_unknown_address_never_verifies(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "stranger@example.com" }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    assert!(sent.lock().unwrap().is_empty());
    let cookie = set_cookie(&res);
    let row: (Option<i32>, Option<String>) =
        sqlx::query_as("SELECT customer_id, code_hash FROM customer_view_codes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (None, None));
    for guess in 0..8 {
        let (_, body) = verify(
            &server,
            &format!("{LOGIN}/verify"),
            &cookie,
            &format!("{guess:06}"),
        )
        .await;
        assert_eq!(body["error"], "That code is not valid.");
    }
    let (_, body) = verify(&server, &format!("{LOGIN}/verify"), &cookie, "999999").await;
    assert_eq!(body["error"], "Too many attempts. Request a new code.");
    assert_eq!(attempts(&pool).await, [8]);
}

/// Before a code is verified the cookie names neither the page nor the
/// address (it is signed, not encrypted): only a random handle.
#[test_db::test(seed)]
async fn pending_cookies_reveal_neither_the_page_nor_the_address(pool: MySqlPool) {
    let (mut server, _) = server(&pool).await;
    server.do_not_save_cookies();
    let check = |cookie: &str| {
        let json = cookie_json(cookie);
        let text = json.to_string();
        assert!(!text.contains(VIEW) && !text.contains("alice"), "{text}");
        assert_eq!(json.as_object().unwrap().len(), 1, "{text}");
        let token = json["pendingCode"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    };
    let page_code = server.post(&page("/code")).await;
    check(&set_cookie(&page_code));
    after_cooldown(&pool).await;
    let login_code = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "Alice.Work@Example.com" }))
        .await;
    let cookie = set_cookie(&login_code);
    check(&cookie);
    // Reading the pending sign-in or a wrong guess writes no address either.
    for res in [
        server.get(LOGIN).add_header("cookie", &cookie).await,
        server
            .post(&format!("{LOGIN}/verify"))
            .add_header("cookie", &cookie)
            .json(&json!({ "code": "x" }))
            .await,
    ] {
        if res.maybe_header("set-cookie").is_some() {
            check(&set_cookie(&res));
        }
    }
    let first = cookie_json(&set_cookie(&page_code))["pendingCode"].clone();
    assert_ne!(first, cookie_json(&cookie)["pendingCode"]);
}

/// The 45 s cooldown and 5 an hour are per address, whichever route asks:
/// the page route and the sign-in by email share them.
#[test_db::test(seed)]
async fn the_page_and_the_sign_in_share_the_limits(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let by_email = || {
        server
            .post(&format!("{LOGIN}/code"))
            .json(&json!({ "email": "alice@example.com" }))
    };
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    by_email()
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    after_cooldown(&pool).await;
    by_email().await.assert_status(StatusCode::NO_CONTENT);
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    for turn in 0..3 {
        after_cooldown(&pool).await;
        let res = if turn % 2 == 0 {
            server.post(&page("/code")).await
        } else {
            by_email().await
        };
        res.assert_status(StatusCode::NO_CONTENT);
    }
    after_cooldown(&pool).await;
    for res in [server.post(&page("/code")).await, by_email().await] {
        res.assert_status(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            res.json::<Value>()["error"],
            "Too many codes were requested. Please try again later."
        );
    }
    assert_eq!(sent.lock().unwrap().len(), 5);
    // Another address of the same customer has its own count.
    server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice.work@example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // An hour later the address may ask again.
    sqlx::query("UPDATE customer_view_codes SET created_at = created_at - INTERVAL 1 HOUR")
        .execute(&pool)
        .await
        .unwrap();
    by_email().await.assert_status(StatusCode::NO_CONTENT);
}

/// Limits are per company: an address that used up its codes at one
/// company still gets one from another.
#[test_db::test(seed)]
async fn limits_are_per_company(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    sqlx::raw_sql(
        "INSERT INTO customers_emails (id, customer_id, email, created_at, created_by) VALUES
           (9901, 2000, 'alice@example.com', '2026-04-01 08:00:00', 106);
         UPDATE customers SET view_id = UUID_TO_BIN('5d3b8e4a-1f6c-4a0d-c3e9-7b4a0f1e2d3c') WHERE id = 2000;",
    )
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..5 {
        server
            .post(&format!("{LOGIN}/code"))
            .json(&json!({ "email": "alice@example.com" }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        after_cooldown(&pool).await;
    }
    server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    let res = server
        .post("/v1/public/customer-view/101/login/code")
        .json(&json!({ "email": "alice@example.com" }))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(sent.lock().unwrap().len(), 6);
    let (status, body) = verify(
        &server,
        "/v1/public/customer-view/101/login/verify",
        &set_cookie(&res),
        &last_code(&sent),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "view_id": "5d3b8e4a-1f6c-4a0d-c3e9-7b4a0f1e2d3c" })
    );
    // And unknown addresses alike.
    server
        .post("/v1/public/customer-view/101/login/code")
        .json(&json!({ "email": "stranger@example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
}

/// When the email cannot be sent the visitor gets 502, and nothing is
/// kept: no code to guess at and no send counted against the address.
#[test_db::test(seed)]
async fn a_failed_email_keeps_no_code_and_spends_no_limit(pool: MySqlPool) {
    let (server, sent, fail) = flaky_server(&pool).await;
    fail.store(true, Ordering::SeqCst);
    for _ in 0..6 {
        let res = server.post(&page("/code")).await;
        res.assert_status(StatusCode::BAD_GATEWAY);
        assert_eq!(
            res.json::<Value>()["error"],
            "We could not send the email. Please try again."
        );
        assert!(res.maybe_header("set-cookie").is_none());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customer_view_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    fail.store(false, Ordering::SeqCst);
    let res = server.post(&page("/code")).await;
    res.assert_status(StatusCode::NO_CONTENT);
    let (status, _) = verify(
        &server,
        &page("/verify"),
        &set_cookie(&res),
        &last_code(&sent),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// A code is good for 10 minutes; after that it says so, and the right
/// code does not open the page.
#[test_db::test(seed)]
async fn an_expired_code_says_so(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server.post(&page("/code")).await;
    let cookie = set_cookie(&res);
    let expires: i64 = sqlx::query_scalar(
        "SELECT TIMESTAMPDIFF(SECOND, created_at, expires_at) FROM customer_view_codes",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expires, 600);
    sqlx::query("UPDATE customer_view_codes SET expires_at = UTC_TIMESTAMP() - INTERVAL 1 SECOND")
        .execute(&pool)
        .await
        .unwrap();
    let gate = server
        .get(&page(""))
        .add_header("cookie", &cookie)
        .await
        .json::<Value>();
    assert_eq!(gate["code_sent"], false);
    for path in [page("/verify"), format!("{LOGIN}/verify")] {
        let (status, body) = verify(&server, &path, &cookie, &last_code(&sent)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "That code expired. Request a new one.");
    }
}

/// Codes expired more than a day ago are deleted on the next send; newer
/// ones stay, so the hourly count still sees them.
#[test_db::test(seed)]
async fn old_codes_are_cleaned_up(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO customer_view_codes (token, company_id, customer_id, email, code_hash, attempts, created_at, expires_at) VALUES
           (REPEAT('a', 64), 100, NULL, 'old@example.com', NULL, 0, UTC_TIMESTAMP() - INTERVAL 3 DAY, UTC_TIMESTAMP() - INTERVAL 2 DAY),
           (REPEAT('b', 64), 100, NULL, 'recent@example.com', NULL, 0, UTC_TIMESTAMP() - INTERVAL 2 HOUR, UTC_TIMESTAMP() - INTERVAL 1 HOUR);",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let emails: Vec<String> =
        sqlx::query_scalar("SELECT email FROM customer_view_codes ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(emails, ["recent@example.com", "alice@example.com"]);
}

/// What the customer types is forgiven as before: spaces inside the code
/// are ignored; anything that is not 6 digits is wrong (and counted).
#[test_db::test(seed)]
async fn codes_are_typed_with_spaces_but_six_digits(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server.post(&page("/code")).await;
    let cookie = set_cookie(&res);
    let code = last_code(&sent);
    let bad = [
        String::new(),
        code[..5].to_string(),
        format!("{code}0"),
        "abcdef".to_string(),
        format!("{}x", &code[..5]),
    ];
    for typed in &bad {
        let (status, body) = verify(&server, &page("/verify"), &cookie, typed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{typed:?}");
        assert_eq!(body["error"], "That code is not valid.");
    }
    assert_eq!(attempts(&pool).await, [5]);
    let spaced = format!(" {} {} {} ", &code[..2], &code[2..4], &code[4..]);
    let (status, _) = verify(&server, &page("/verify"), &cookie, &spaced).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// "Resend" (an empty email) goes again to the address of the pending
/// sign-in, and the new code replaces the first.
#[test_db::test(seed)]
async fn a_resend_goes_to_the_pending_address(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    server.do_not_save_cookies();
    let res = server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "Alice.Work@Example.com" }))
        .await;
    let first_cookie = set_cookie(&res);
    let first_code = last_code(&sent);
    after_cooldown(&pool).await;
    let res = server
        .post(&format!("{LOGIN}/code"))
        .add_header("cookie", &first_cookie)
        .json(&json!({}))
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    let cookie = set_cookie(&res);
    assert_eq!(
        sent.lock().unwrap()[1].to,
        vec!["alice.work@example.com".to_string()]
    );
    let (_, body) = verify(
        &server,
        &format!("{LOGIN}/verify"),
        &first_cookie,
        &first_code,
    )
    .await;
    assert_eq!(body["error"], "That code expired. Request a new one.");
    let (status, body) = verify(
        &server,
        &format!("{LOGIN}/verify"),
        &cookie,
        &last_code(&sent),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "view_id": VIEW }));
    // Without a pending sign-in, an empty email is refused.
    server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "" }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// Once verified, nothing is pending any more and the cookie carries the
/// verified page only.
#[test_db::test(seed)]
async fn verifying_clears_the_pending_code(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    server
        .post(&page("/code"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .post(&page("/verify"))
        .json(&json!({ "code": last_code(&sent) }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let gate = server.get(&page("")).await.json::<Value>();
    assert_eq!(gate["verified"], true);
    assert_eq!(gate["code_sent"], false);
    assert_eq!(
        server.get(LOGIN).await.json::<Value>(),
        json!({ "code_sent": false, "masked_email": null })
    );
}

/// Visitors verified before the change keep their page for 7 days: the
/// verified fields of the cookie are read as before.
#[test_db::test(seed)]
async fn verified_cookies_still_open_the_page_for_seven_days(pool: MySqlPool) {
    let (mut server, _) = server(&pool).await;
    server.do_not_save_cookies();
    let verified = |at: i64| {
        signed(
            &json!({ "verifiedViewId": VIEW, "verifiedCompanyId": 100, "verifiedAt": at }),
            SECRET,
        )
    };
    let day = 24 * 60 * 60 * 1000;
    let fresh = verified(now_ms() - 6 * day);
    server
        .get(&page("/projects"))
        .add_header("cookie", &fresh)
        .await
        .assert_status_ok();
    let gate = server
        .get(&page(""))
        .add_header("cookie", &fresh)
        .await
        .json::<Value>();
    assert_eq!(gate["verified"], true);
    assert_eq!(gate["customer_name"], "Alice Lead");
    for cookie in [
        verified(now_ms() - 8 * day),
        signed(
            &json!({ "verifiedViewId": VIEW, "verifiedCompanyId": 101, "verifiedAt": now_ms() }),
            SECRET,
        ),
        signed(
            &json!({ "verifiedViewId": VIEW, "verifiedCompanyId": 100, "verifiedAt": now_ms() }),
            "not-the-secret",
        ),
    ] {
        server
            .get(&page("/projects"))
            .add_header("cookie", &cookie)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
}

/// A sign-in pending at one company is not pending at another.
#[test_db::test(seed)]
async fn a_pending_sign_in_belongs_to_its_company(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(server.get(LOGIN).await.json::<Value>()["code_sent"], true);
    assert_eq!(
        server
            .get("/v1/public/customer-view/101/login")
            .await
            .json::<Value>(),
        json!({ "code_sent": false, "masked_email": null })
    );
}

/// After "use another email" the code no longer verifies in this browser.
#[test_db::test(seed)]
async fn a_cancelled_code_does_not_verify(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    server
        .post(&format!("{LOGIN}/code"))
        .json(&json!({ "email": "alice@example.com" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete("/v1/public/customer-view/login")
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let res = server
        .post(&format!("{LOGIN}/verify"))
        .json(&json!({ "code": last_code(&sent) }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json::<Value>()["error"], "That code is not valid.");
}

/// A page whose customer has no email cannot get a code (409), and nothing
/// is recorded.
#[test_db::test(seed)]
async fn a_page_without_an_email_is_locked(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    sqlx::query(
        "UPDATE customers SET view_id = UUID_TO_BIN('6e4c9f5b-2a7d-4b1e-d4fa-8c5b1a2f3e4d') WHERE id = 1005",
    )
    .execute(&pool)
    .await
    .unwrap();
    let path = "/v1/public/customer-view/100/6e4c9f5b-2a7d-4b1e-d4fa-8c5b1a2f3e4d";
    let gate = server.get(path).await.json::<Value>();
    assert_eq!(gate["has_email"], false);
    let res = server.post(&format!("{path}/code")).await;
    res.assert_status(StatusCode::CONFLICT);
    assert!(sent.lock().unwrap().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM customer_view_codes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[test_db::test(seed)]
async fn sending_an_older_cookie_again_does_not_reset_the_attempts(pool: MySqlPool) {
    let (mut server, sent) = server(&pool).await;
    let requested = server.post(&page("/code")).await;
    requested.assert_status(StatusCode::NO_CONTENT);
    // The cookie right after the email: no failed attempts in it yet.
    let fresh = requested.cookie("__customer_view");
    server.do_not_save_cookies();
    server.clear_cookies();
    let code = last_code(&sent);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..8 {
        server
            .post(&page("/verify"))
            .add_cookie(fresh.clone())
            .json(&json!({ "code": wrong }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let res = server
        .post(&page("/verify"))
        .add_cookie(fresh)
        .json(&json!({ "code": code }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Too many attempts. Request a new code."
    );
}
