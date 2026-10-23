//! A customer's own page: unlocked by an emailed one-time code (by page or
//! by the account's email), then their projects and invoices.

mod common;

use api::app;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET};
use serde_json::{Value, json};
use sqlx::MySqlPool;
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
