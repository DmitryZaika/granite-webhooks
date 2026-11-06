//! The app opens CRM pages (the quote editor) signed in: `POST
//! /v1/auth/web-link` with the device token, then the browser follows
//! `GET /v1/auth/web-link/{token}` to the page with a session cookie.
//!
//! Fixture phones use n = 321..=329.

use crate::common;
use crate::phone_common;

use api::auth::cookie::CookieConfig;
use axum::http::StatusCode;
use axum_test::{TestResponse, TestServer};
use common::{WithSession, sessions};
use phone_common::{HEADER, add_session, phone_server_with, revoke_session, sql, token};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const WEB: &str = "https://granite-manager.com";

async fn server(pool: &MySqlPool) -> TestServer {
    sql(pool, "UPDATE company SET subdomain = 'acme' WHERE id = 100").await;
    phone_server_with(pool.clone(), |state| {
        state.with_app_url(WEB).with_cookie(CookieConfig {
            domain: Some(".granite-manager.com".into()),
            secure: true,
        })
    })
    .await
    .0
}

/// The link phone `n` gets for `path`: its path on the API.
async fn link(server: &TestServer, n: u32, path: &str) -> String {
    let response = server
        .post("/v1/auth/web-link")
        .add_header(HEADER, token(n))
        .json(&json!({ "path": path }))
        .await;
    response.assert_status_ok();
    let body: Value = response.json();
    assert_eq!(body["expires_in"], 120);
    body["link_path"].as_str().unwrap().to_string()
}

fn location(response: &TestResponse) -> String {
    response.assert_status(StatusCode::SEE_OTHER);
    response.headers()["location"].to_str().unwrap().to_string()
}

/// The `__session=...` value the response set, if any.
fn session_cookie(response: &TestResponse) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .rfind(|v| v.starts_with("__session=") && !v.starts_with("__session=;"))
        .map(|v| v.split(';').next().unwrap().to_string())
}

async fn sessions_of(pool: &MySqlPool, user: i32) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE user_id = ?")
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn the_phone_opens_a_quote_in_the_crm_signed_in(pool: MySqlPool) {
    // Rita (user 100, company 100 on acme) signed in on her phone.
    add_session(&pool, 321, 100, 100).await;
    let server = server(&pool).await;
    let path = link(&server, 321, "/employee/quote/12").await;
    assert!(path.starts_with("/v1/auth/web-link/"), "{path}");

    // A browser signed out lands on the quote, on her company's host, signed
    // in as Rita: the cookie it got works on the CRM's routes.
    let before = sessions_of(&pool, 100).await;
    let opened = server.get(&path).await;
    assert_eq!(
        location(&opened),
        "https://acme.granite-manager.com/employee/quote/12"
    );
    assert_eq!(opened.headers()["cache-control"], "no-store");
    assert_eq!(opened.headers()["referrer-policy"], "no-referrer");
    let set = opened
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        set.iter().any(|c| !c.starts_with("__session=;")
            && c.contains("HttpOnly")
            && c.contains("Secure")
            && c.contains("Domain=.granite-manager.com")),
        "{set:?}"
    );
    let cookie = session_cookie(&opened).expect("a session cookie");
    assert_eq!(sessions_of(&pool, 100).await, before + 1);
    let me = server
        .get("/v1/auth/session")
        .add_header("cookie", cookie.clone())
        .await;
    me.assert_status_ok();
    assert_eq!(me.json::<Value>()["user"]["id"], 100);

    // Opened again in that browser, it keeps its session: no new one.
    let again = server
        .get(&link(&server, 321, "/employee/quotes/add").await)
        .add_header("cookie", cookie)
        .await;
    assert_eq!(
        location(&again),
        "https://acme.granite-manager.com/employee/quotes/add"
    );
    assert_eq!(session_cookie(&again), None);
    assert_eq!(sessions_of(&pool, 100).await, before + 1);

    // A browser signed in as someone else is signed in as Rita instead.
    let other = server
        .get(&link(&server, 321, "/employee/quote/12").await)
        .with_session(sessions::MANAGER)
        .await;
    location(&other);
    let cookie = session_cookie(&other).expect("Rita's cookie");
    let me = server
        .get("/v1/auth/session")
        .add_header("cookie", cookie)
        .await;
    assert_eq!(me.json::<Value>()["user"]["id"], 100);
}

#[test_db::test(seed)]
async fn a_link_only_works_while_it_is_good(pool: MySqlPool) {
    add_session(&pool, 322, 100, 100).await;
    let server = server(&pool).await;

    // Only pages of the CRM's own host, and only with the device token.
    for path in [
        "https://evil.example/x",
        "//evil.example",
        "/\\evil",
        "quote/1",
        "",
    ] {
        server
            .post("/v1/auth/web-link")
            .add_header(HEADER, token(322))
            .json(&json!({ "path": path }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    server
        .post("/v1/auth/web-link")
        .with_session(sessions::REP)
        .json(&json!({ "path": "/employee/quote/12" }))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // A changed link goes to the sign-in page and signs nobody in.
    let path = link(&server, 322, "/employee/quote/12").await;
    let forged = format!("{path}x");
    let refused = server.get(&forged).await;
    assert_eq!(location(&refused), format!("{WEB}/login"));
    assert_eq!(session_cookie(&refused), None);

    // So does a good link once its phone is signed out.
    revoke_session(&pool, 322).await;
    let refused = server.get(&path).await;
    assert_eq!(location(&refused), format!("{WEB}/login"));
    assert_eq!(session_cookie(&refused), None);

    // ... or once its user is deleted.
    add_session(&pool, 323, 100, 100).await;
    let path = link(&server, 323, "/employee/quote/12").await;
    sql(&pool, "UPDATE users SET is_deleted = 1 WHERE id = 100").await;
    let refused = server.get(&path).await;
    assert_eq!(location(&refused), format!("{WEB}/login"));
    assert_eq!(session_cookie(&refused), None);
}
