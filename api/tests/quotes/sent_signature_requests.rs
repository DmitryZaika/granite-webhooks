//! Documents sent for signature: the sender's list, the company's list for
//! admins, and cancelling (deleted at `SignWell`, revoked when native).

use crate::common;

use api::app;
use api::state::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = "
UPDATE company SET signwell_api_key = 'sw-key' WHERE id = 100;
INSERT INTO signature_requests (id, company_id, customer_id, created_by_user_id, filename, file_url,
    signer_name, signer_email, status, provider, provider_document_id, created_at)
VALUES
  (9301, 100, 1000, 100, 'Quote.pdf', 'https://files.test/q.pdf', 'John Clay', 'john@example.com', 'sent', 'signwell', 'sw-open', '2026-10-01 10:00:00'),
  (9302, 100, 1000, 100, 'Done.pdf', 'https://files.test/d.pdf', 'John Clay', 'john@example.com', 'signed', 'signwell', 'sw-done', '2026-10-02 10:00:00'),
  (9303, 100, 1000, 101, 'Other.pdf', 'https://files.test/o.pdf', 'Ann Stone', 'ann@example.com', 'declined', 'signwell', 'sw-other', '2026-10-03 10:00:00');
";

async fn server(pool: &MySqlPool) -> (TestServer, Arc<Mutex<Vec<String>>>) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let seen = deleted.clone();
    let router = Router::new().route(
        "/documents/{id}",
        get(|Path(id): Path<String>| async move {
            let status = if id == "sw-done" { "Completed" } else { "Sent" };
            Json(json!({ "status": status, "recipients": [{ "status": "sent" }] }))
        })
        .delete(move |Path(id): Path<String>| async move {
            seen.lock().unwrap().push(id);
            StatusCode::NO_CONTENT
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_signwell_base(&format!("http://{address}"));
    (TestServer::new(app(state)), deleted)
}

fn ids(rows: &Value) -> Vec<i64> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn senders_see_theirs_and_admins_see_the_company(pool: MySqlPool) {
    let (server, _) = server(&pool).await;
    let res = server
        .get("/v1/signature-requests")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let mine = res.json::<Value>();
    assert_eq!(ids(&mine), vec![9302, 9301]);
    assert!(mine[0]["customer_name"].is_string());
    assert_eq!(
        mine[0]["signed_url"],
        "/v1/signature-requests/9302/signed-pdf"
    );
    assert_eq!(mine[1]["status"], "sent");

    server
        .get("/v1/signature-requests?scope=company")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let res = server
        .get("/v1/signature-requests?scope=company")
        .with_session(sessions::ADMIN)
        .await;
    res.assert_status_ok();
    assert_eq!(ids(&res.json::<Value>()), vec![9303, 9302, 9301]);
    server
        .get("/v1/signature-requests?scope=everyone")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .get("/v1/signature-requests")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn cancelling_deletes_the_signwell_document(pool: MySqlPool) {
    let (server, deleted) = server(&pool).await;
    server
        .post("/v1/signature-requests/9301/cancel")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(*deleted.lock().unwrap(), vec!["sw-open".to_string()]);
    let status: String =
        sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM signature_requests WHERE id = 9301")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "expired");

    // Finished, or someone else's: refused.
    let res = server
        .post("/v1/signature-requests/9302/cancel")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "This request is already finished."
    );
    server
        .post("/v1/signature-requests/9303/cancel")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn cancelling_needs_the_signwell_key(pool: MySqlPool) {
    let (server, deleted) = server(&pool).await;
    sqlx::raw_sql("UPDATE company SET signwell_api_key = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let res = server
        .post("/v1/signature-requests/9301/cancel")
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        res.json::<Value>()["error"]
            .as_str()
            .unwrap()
            .contains("SignWell API key")
    );
    assert!(deleted.lock().unwrap().is_empty());
}

#[test_db::test(seed)]
async fn cancelling_can_tell_the_customer(pool: MySqlPool) {
    sqlx::raw_sql(FIXTURE).execute(&pool).await.unwrap();
    let sent: Arc<Mutex<Vec<api::webhooks_client::OutgoingEmail>>> = Arc::default();
    let recorded = sent.clone();
    let router = Router::new()
        .route(
            "/documents/{id}",
            axum::routing::delete(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/email/send",
            axum::routing::post(
                move |Json(email): Json<api::webhooks_client::OutgoingEmail>| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(email);
                        Json(json!({ "message_id": "<c@ses.test>" }))
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_signwell_base(&base)
        .with_webhooks(api::webhooks_client::WebhooksClient::new(
            Some(&base),
            Some("key"),
        ));
    let server = TestServer::new(app(state));
    let res = server
        .post("/v1/signature-requests/9301/cancel")
        .with_session(sessions::REP)
        .json(&json!({ "notify_customer": true }))
        .await;
    res.assert_status(StatusCode::OK);
    assert_eq!(res.json::<Value>()["customer_notified"], true);
    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["john@example.com"]);
    assert_eq!(sent[0].subject, "Signature request canceled: Quote.pdf");
    assert!(sent[0].body.contains("Hi John,"), "{}", sent[0].body);
}
