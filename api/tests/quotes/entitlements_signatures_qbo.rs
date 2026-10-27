use crate::common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
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

// Company 100 is a legacy company (every add-on). 20100 is on a subscription
// with Quotes only; 20101 with a comp Signatures grant only.

const QUICKBOOKS_403: &str =
    r#"{"error":"QuickBooks is an add-on for your plan.","addon":"quickbooks"}"#;

fn quickbooks_required() -> Value {
    serde_json::from_str(QUICKBOOKS_403).unwrap()
}

fn signatures_required() -> Value {
    json!({ "error": "Signatures is an add-on for your plan.", "addon": "signatures" })
}

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn fake_send(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    let mut sent = sent.lock().unwrap();
    sent.push(email);
    Json(json!({ "message_id": format!("010f-test-{}@us-east-2.amazonses.com", sent.len()) }))
}

async fn mail_server(pool: &MySqlPool) -> (TestServer, Sent) {
    let sent: Sent = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(Arc::clone(&sent));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("webhooks-key"),
        ));
    (TestServer::new(app(state)), sent)
}

async fn save_signature(pool: &MySqlPool, user_id: i32) {
    sqlx::query("UPDATE users SET email_signature = '<p>Saved sig</p>' WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn send(server: &TestServer, session: &str) {
    server
        .post("/v1/emails")
        .with_session(session)
        .json(&json!({
            "to": ["buyer@example.com"],
            "subject": "Hello",
            "body": "<p>Hi</p>",
        }))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn legacy_company_keeps_everything(pool: MySqlPool) {
    save_signature(&pool, 107).await;
    let (server, sent) = mail_server(&pool).await;
    server
        .get("/v1/invoice-queue")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    server
        .get("/v1/signature-requests")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    let account: Value = server
        .get("/v1/me/account")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(account["shows_email_signature"], true);
    send(&server, sessions::DEALER).await;
    let sent = sent.lock().unwrap();
    assert_eq!(sent[0].signature.as_deref(), Some("<p>Saved sig</p>"));
}

#[test_db::test(seed)]
async fn quotes_only_company_has_neither_quickbooks_nor_signatures(pool: MySqlPool) {
    save_signature(&pool, 20100).await;
    let (server, sent) = mail_server(&pool).await;
    let manager = sessions::QUOTES_ONLY_MANAGER;

    let queue = server.get("/v1/invoice-queue").with_session(manager).await;
    queue.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(queue.json::<Value>(), quickbooks_required());
    let one = server
        .get("/v1/invoice-queue/7010")
        .with_session(manager)
        .await;
    one.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(one.json::<Value>(), quickbooks_required());

    let customers = server
        .get("/v1/qbo/customers?email=a@example.com")
        .with_session(manager)
        .await;
    customers.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(customers.json::<Value>(), quickbooks_required());
    let class_sales = server
        .get("/v1/qbo/class-sales")
        .with_session(manager)
        .await;
    class_sales.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(class_sales.json::<Value>()["addon"], "quickbooks");

    let requests = server
        .get("/v1/signature-requests")
        .with_session(manager)
        .await;
    requests.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(requests.json::<Value>(), signatures_required());

    let account: Value = server
        .get("/v1/me/account")
        .with_session(manager)
        .await
        .json();
    assert_eq!(account["shows_email_signature"], false);
    server
        .put("/v1/me/account")
        .with_session(manager)
        .json(&json!({
            "name": "Quinn Manager",
            "email": "manager@quotes-only.test",
            "email_signature": "<p>New</p>",
        }))
        .await
        .assert_status_ok();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT email_signature FROM users WHERE id = 20100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("<p>Saved sig</p>"));

    send(&server, manager).await;
    assert_eq!(sent.lock().unwrap()[0].signature, None);
}

#[test_db::test(seed)]
async fn comp_signatures_company_has_signatures_only(pool: MySqlPool) {
    save_signature(&pool, 20103).await;
    let (server, sent) = mail_server(&pool).await;
    server
        .get("/v1/signature-requests")
        .with_session(sessions::COMP_SIGNATURES_REP)
        .await
        .assert_status_ok();
    let account: Value = server
        .get("/v1/me/account")
        .with_session(sessions::COMP_SIGNATURES_REP)
        .await
        .json();
    assert_eq!(account["shows_email_signature"], true);
    send(&server, sessions::COMP_SIGNATURES_REP).await;
    assert_eq!(
        sent.lock().unwrap()[0].signature.as_deref(),
        Some("<p>Saved sig</p>")
    );
    let queue = server
        .get("/v1/invoice-queue")
        .with_session(sessions::COMP_SIGNATURES_MANAGER)
        .await;
    queue.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(queue.json::<Value>(), quickbooks_required());
}

#[test_db::test(seed)]
async fn the_invoice_queue_never_crosses_companies(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_admin = 1 WHERE id = 106")
        .execute(&pool)
        .await
        .unwrap();
    let (server, _) = mail_server(&pool).await;
    // Sale 7010 belongs to company 100.
    server
        .get("/v1/invoice-queue/7010")
        .with_session(sessions::QUOTES_ONLY_MANAGER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/invoice-queue/7010")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
