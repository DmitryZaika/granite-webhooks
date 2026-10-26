use crate::common;

use api::app;
use api::auth::cookie::CookieConfig;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const PASSWORD: &str = "granite-local";

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn fake_send(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    sent.lock().unwrap().push(email);
    Json(json!({ "message_id": "<m@test>" }))
}

async fn setup(pool: MySqlPool, domain: Option<&str>) -> (TestServer, Sent) {
    let sent: Sent = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(sent.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("key"),
        ))
        .with_cookie(CookieConfig {
            domain: domain.map(str::to_string),
            secure: true,
        })
        .with_app_url("https://granite-manager.com");
    (TestServer::new(app(state)), sent)
}

/// `name=value` of the session cookie a response set (the last `__session`).
fn session_cookie(response: &axum_test::TestResponse) -> String {
    let set = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .rfind(|v| v.starts_with("__session=") && !v.starts_with("__session=;"))
        .expect("session cookie")
        .to_string();
    set.split(';').next().unwrap().to_string()
}

#[test_db::test(seed)]
async fn a_user_signs_in_and_out(pool: MySqlPool) {
    let (server, _) = setup(pool.clone(), Some(".granite-manager.com")).await;
    let response = server
        .post("/v1/auth/login")
        .json(&json!({ "email": "  REP@acme.test ", "password": PASSWORD }))
        .await;
    response.assert_status_ok();
    let cookies: Vec<String> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert!(cookies.iter().any(|c| c.contains("HttpOnly")
        && c.contains("Secure")
        && c.contains("SameSite=Lax")
        && c.contains("Domain=.granite-manager.com")
        && !c.starts_with("__session=;")));
    let body: Value = response.json();
    assert_eq!(body["user"]["id"], 100);
    assert_eq!(body["home_path"], "/employee");
    let cookie = session_cookie(&response);

    // The cookie signs in the other routes.
    server
        .get("/v1/me")
        .add_header("cookie", cookie.clone())
        .await
        .assert_status_ok();
    let session: Value = server
        .get("/v1/auth/session")
        .add_header("cookie", cookie.clone())
        .await
        .json();
    assert_eq!(session["user"]["email"], "rep@acme.test");

    let out = server
        .post("/v1/auth/logout")
        .add_header("cookie", cookie.clone())
        .await;
    out.assert_status(StatusCode::NO_CONTENT);
    assert!(
        out.headers()
            .get_all("set-cookie")
            .iter()
            .all(|v| v.to_str().unwrap().contains("Max-Age=0"))
    );
    // A replayed cookie no longer works.
    server
        .get("/v1/me")
        .add_header("cookie", cookie)
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn wrong_passwords_are_counted_and_limited(pool: MySqlPool) {
    let (server, _) = setup(pool.clone(), None).await;
    for email in ["rep@acme.test", "nobody@acme.test"] {
        let response = server
            .post("/v1/auth/login")
            .json(&json!({ "email": email, "password": "wrong-password" }))
            .await;
        response.assert_status_unauthorized();
        assert_eq!(
            response.json::<Value>()["error"],
            "Incorrect email or password. Please try again."
        );
    }
    for _ in 0..9 {
        server
            .post("/v1/auth/login")
            .json(&json!({ "email": "rep@acme.test", "password": "wrong-password" }))
            .await
            .assert_status_unauthorized();
    }
    // Ten failures from this address: even the right password waits.
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD }))
        .await
        .assert_status(StatusCode::TOO_MANY_REQUESTS);
    // Another address still gets in, and success clears the count.
    server
        .post("/v1/auth/login")
        .add_header("x-forwarded-for", "203.0.113.9")
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD }))
        .await
        .assert_status_ok();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 1);
    // Deleted users cannot sign in.
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "gone@acme.test", "password": PASSWORD }))
        .await
        .assert_status_unauthorized();
}

#[test_db::test(seed)]
async fn writes_from_other_sites_are_refused(pool: MySqlPool) {
    let (server, _) = setup(pool, None).await;
    let response = server
        .post("/v1/auth/login")
        .add_header("origin", "https://evil.example")
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD }))
        .await;
    response.assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/auth/login")
        .add_header("origin", ALLOWED_ORIGIN)
        .json(&json!({ "email": "rep@acme.test", "password": PASSWORD }))
        .await
        .assert_status_ok();
    // Reads are not affected.
    server
        .get("/v1/me")
        .add_header("origin", "https://evil.example")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn portal_users_start_in_their_portal(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 6, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, _) = setup(pool, None).await;
    let body: Value = server
        .post("/v1/auth/login")
        .json(&json!({ "email": "manager@acme.test", "password": PASSWORD }))
        .await
        .json();
    assert_eq!(body["home_path"], "/installers/100/checklist");
}

#[test_db::test(seed)]
async fn super_admins_switch_company_and_keep_the_cookie(pool: MySqlPool) {
    let (server, _) = setup(pool, None).await;
    let login = server
        .post("/v1/auth/login")
        .json(&json!({ "email": "super@other.test", "password": PASSWORD }))
        .await;
    let body: Value = login.json();
    let companies: Vec<i64> = body["switchable_companies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    assert_eq!(companies.len(), 2);
    let cookie = session_cookie(&login);
    let switched = server
        .put("/v1/auth/active-company")
        .add_header("cookie", cookie.clone())
        .json(&json!({ "company_id": 100 }))
        .await;
    switched.assert_status_ok();
    let cookie = session_cookie(&switched);
    let me: Value = server
        .get("/v1/me")
        .add_header("cookie", cookie.clone())
        .await
        .json();
    assert_eq!(me["company_id"], 100);
    // Only companies the caller is super admin of.
    server
        .put("/v1/auth/active-company")
        .add_header("cookie", cookie)
        .json(&json!({ "company_id": 999 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/auth/active-company")
        .with_session(sessions::REP)
        .json(&json!({ "company_id": 100 }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_password_is_reset_by_email(pool: MySqlPool) {
    sqlx::query("UPDATE company SET subdomain = 'acme' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let (server, sent) = setup(pool.clone(), None).await;
    // Unknown emails get the same answer and no email.
    server
        .post("/v1/auth/password-reset")
        .json(&json!({ "email": "nobody@acme.test" }))
        .await
        .assert_status(StatusCode::ACCEPTED);
    assert!(sent.lock().unwrap().is_empty());
    server
        .post("/v1/auth/password-reset")
        .add_header("host", "evil.example")
        .add_header("x-forwarded-host", "evil.example")
        .json(&json!({ "email": "Rep@Acme.test" }))
        .await
        .assert_status(StatusCode::ACCEPTED);
    // A second request within a minute sends nothing more.
    server
        .post("/v1/auth/password-reset")
        .json(&json!({ "email": "rep@acme.test" }))
        .await
        .assert_status(StatusCode::ACCEPTED);
    let emails = sent.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, ["rep@acme.test"]);
    let body = &emails[0].body;
    let start = body
        .find("https://acme.granite-manager.com/reset-password/")
        .unwrap();
    let token: String = body[start + "https://acme.granite-manager.com/reset-password/".len()..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    assert_eq!(token.len(), 96);
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM password_reset_tokens")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, token);

    server
        .get(&format!("/v1/auth/password-reset/{token}"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .post(&format!("/v1/auth/password-reset/{token}"))
        .json(&json!({ "password": "short" }))
        .await
        .assert_status_bad_request();
    server
        .post(&format!("/v1/auth/password-reset/{token}"))
        .json(&json!({ "password": "  a-new-password " }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    // Every session of the user ended; the link is spent.
    server
        .get("/v1/me")
        .with_session(sessions::REP)
        .await
        .assert_status_unauthorized();
    server
        .post(&format!("/v1/auth/password-reset/{token}"))
        .json(&json!({ "password": "another-password" }))
        .await
        .assert_status_not_found();
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "rep@acme.test", "password": "a-new-password" }))
        .await
        .assert_status_ok();
    server
        .get("/v1/auth/password-reset/not-a-token")
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn an_invited_user_sets_up_their_own_account(pool: MySqlPool) {
    sqlx::query("UPDATE company SET subdomain = 'acme' WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    // Ada (102) adds users once she is a Manager of 100.
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (102, 5, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, sent) = setup(pool.clone(), None).await;
    let res = server
        .post("/v1/admin/users")
        .with_session(sessions::ADMIN)
        .json(&json!({ "email": " New@Acme.test ", "positions": [1] }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let created: Value = res.json();
    assert_eq!(created["email_sent"], true);
    let url = created["invite_url"].as_str().unwrap().to_string();
    let token = url
        .strip_prefix("https://acme.granite-manager.com/invite/")
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 96);
    let emails = sent.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, ["new@acme.test"]);
    assert!(emails[0].subject.contains("Acme Stone"));
    assert!(emails[0].body.contains(&url));

    let invite: Value = server.get(&format!("/v1/auth/invite/{token}")).await.json();
    assert_eq!(
        invite,
        json!({ "email": "new@acme.test", "company_name": "Acme Stone" })
    );
    // No password yet: nothing signs in.
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "new@acme.test", "password": "a-new-password" }))
        .await
        .assert_status_unauthorized();
    for body in [
        json!({ "name": " ", "phone_number": "3175550123", "password": "a-new-password" }),
        json!({ "name": "Nia New", "phone_number": "555-0123", "password": "a-new-password" }),
        json!({ "name": "Nia New", "phone_number": "3175550123", "password": "short" }),
    ] {
        server
            .post(&format!("/v1/auth/invite/{token}"))
            .json(&body)
            .await
            .assert_status_bad_request();
    }

    let res = server
        .post(&format!("/v1/auth/invite/{token}"))
        .json(&json!({
            "name": " Nia New ",
            "phone_number": " 317-555-0123 ",
            "password": " a-new-password ",
        }))
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["user"]["email"], "new@acme.test");
    assert_eq!(body["user"]["name"], "Nia New");
    assert_eq!(body["home_path"], "/employee");
    // Signed in by the answer's cookie.
    server
        .get("/v1/me")
        .add_header("cookie", session_cookie(&res))
        .await
        .assert_status_ok();
    let (phone, hash): (String, String) =
        sqlx::query_as("SELECT phone_number, password FROM users WHERE email = 'new@acme.test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(phone, "317-555-0123");
    assert!(bcrypt::verify("a-new-password", &hash).unwrap());

    // The link works once.
    server
        .get(&format!("/v1/auth/invite/{token}"))
        .await
        .assert_status_not_found();
    server
        .post(&format!("/v1/auth/invite/{token}"))
        .json(&json!({
            "name": "Someone Else",
            "phone_number": "3175550199",
            "password": "another-password",
        }))
        .await
        .assert_status_not_found();
    server
        .post("/v1/auth/login")
        .json(&json!({ "email": "new@acme.test", "password": "a-new-password" }))
        .await
        .assert_status_ok();

    // A reset link is no invite: that account already has a password.
    server
        .post("/v1/auth/password-reset")
        .json(&json!({ "email": "rep@acme.test" }))
        .await
        .assert_status(StatusCode::ACCEPTED);
    let reset = sent.lock().unwrap().last().unwrap().body.clone();
    let start = reset.find("/reset-password/").unwrap() + "/reset-password/".len();
    let reset_token: String = reset[start..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    assert_eq!(reset_token.len(), 96);
    server
        .get(&format!("/v1/auth/invite/{reset_token}"))
        .await
        .assert_status_not_found();
    server
        .get("/v1/auth/invite/not-a-token")
        .await
        .assert_status_not_found();
}
