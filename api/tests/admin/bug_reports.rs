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

type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;

async fn fake_send(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
    sent.lock().unwrap().push(email);
    Json(json!({ "message_id": "<m@test>" }))
}

async fn setup(pool: MySqlPool) -> (TestServer, Storage, Sent) {
    let sent: Sent = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(sent.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let storage = Storage::memory();
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_storage(storage.clone())
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("webhooks-key"),
        ));
    (TestServer::new(app(state)), storage, sent)
}

#[test_db::test(seed)]
async fn employees_report_bugs_and_the_team_hears(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 103")
        .execute(&pool)
        .await
        .unwrap();
    let (server, _, sent) = setup(pool).await;
    let res = server
        .post("/v1/uploads")
        .with_session(sessions::REP)
        .json(&json!({ "folder": "bug-reports", "file_name": "log.txt", "content_type": "text/plain" }))
        .await;
    res.assert_status(StatusCode::CREATED);
    let url = res.json::<Value>()["url"].as_str().unwrap().to_string();
    let res = server
        .post("/v1/bug-reports")
        .with_session(sessions::REP)
        .json(&json!({ "description": "Save <fails>\nwhen I click", "file_urls": [url] }))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>()["notified"], true);
    let emails = sent.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, ["dema@granitedepotindy.com"]);
    assert_eq!(emails[0].reply_to, ["rep@acme.test"]);
    assert_eq!(emails[0].subject, "[Bug Report] Save <fails>");
    assert!(emails[0].body.contains("Save &lt;fails&gt;"));
    assert!(emails[0].body.contains(&url));
    assert!(emails[0].transactional);

    // Super admins read them all.
    server
        .get("/v1/bug-reports")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let super_admin = sessions::SUPER_ADMIN;
    let reports: Value = server
        .get("/v1/bug-reports")
        .with_session(super_admin)
        .await
        .json();
    assert_eq!(reports[0]["title"], "Save <fails>");
    assert_eq!(reports[0]["user_name"], "Rita Rep");
    assert_eq!(reports[0]["image_urls"], json!([url]));
}

#[test_db::test(seed)]
async fn bug_reports_need_a_description_and_own_files(pool: MySqlPool) {
    let (server, _, sent) = setup(pool).await;
    server
        .post("/v1/bug-reports")
        .with_session(sessions::REP)
        .json(&json!({ "description": "  " }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/bug-reports")
        .with_session(sessions::REP)
        .json(&json!({ "description": "x", "file_urls": ["https://elsewhere.test/a.png"] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert!(sent.lock().unwrap().is_empty());
}
