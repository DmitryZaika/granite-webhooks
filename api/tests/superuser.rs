mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn make_super(pool: &MySqlPool) {
    sqlx::raw_sql(
        "UPDATE users SET is_superuser = 1 WHERE id = 103;
         UPDATE company SET signature_enabled = 1 WHERE id = 100;
         UPDATE company SET signature_enabled = 0 WHERE id = 101;
         INSERT INTO onboarding_feedback (id, user_id, company_id, rating, feedback_text, created_at) VALUES
           (9400, 100, 100, 4, 'Nice tours', '2026-09-01 10:00:00');",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn super_admins_read_onboarding_feedback(pool: MySqlPool) {
    make_super(&pool).await;
    let server = server(pool);
    let rows: Value = server
        .get("/v1/superuser/onboarding-feedbacks")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(rows[0]["rating"], 4);
    assert_eq!(rows[0]["user_name"], "Rita Rep");
    assert_eq!(rows[0]["created_at"], "2026-09-01T10:00:00.000Z");
    server
        .get("/v1/superuser/onboarding-feedbacks")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn super_admins_switch_signature_access(pool: MySqlPool) {
    make_super(&pool).await;
    let server = server(pool.clone());
    let body: Value = server
        .get("/v1/superuser/signature-access?company_id=101")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    // Company 101 has signatures off: not offered, so the filter is dropped.
    assert_eq!(body["company_id"], Value::Null);
    let companies: Vec<i64> = body["companies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|company| company["id"].as_i64().unwrap())
        .collect();
    assert!(companies.contains(&100) && !companies.contains(&101));
    let rita = body["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["id"] == 100)
        .unwrap();
    assert_eq!(rita["is_signature_available"], false);
    server
        .put("/v1/superuser/users/100/signature-access")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let on: bool =
        sqlx::query_scalar("SELECT is_signature_available <> 0 FROM users WHERE id = 100")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(on);
    server
        .put("/v1/superuser/users/105/signature-access")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "enabled": true }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .put("/v1/superuser/users/100/signature-access")
        .with_session(sessions::ADMIN)
        .json(&json!({ "enabled": false }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn super_admins_read_widget_analytics(pool: MySqlPool) {
    make_super(&pool).await;
    sqlx::query(
        "INSERT INTO widget_analytics_events (user_id, company_id, widget_id, action, target) VALUES
           (100, 100, 'leaderboard', 'click', 'row'), (100, 100, 'leaderboard', 'click', 'row'),
           (100, 100, 'leaderboard', 'add', 'dashboard')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_dashboard_widgets (user_id, company_id, widget_id, position)
         VALUES (100, 100, 'leaderboard', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/superuser/widget-analytics")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(body["top_used"][0]["uses"], 2);
    assert_eq!(body["events"].as_array().unwrap().len(), 3);
    assert_eq!(body["current_widgets"][0]["widget_id"], "leaderboard");
    server
        .get("/v1/superuser/widget-analytics")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn super_admins_send_a_plain_email(pool: MySqlPool) {
    use api::app;
    use api::state::AppState;
    use api::webhooks_client::{OutgoingEmail, WebhooksClient};
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use axum_test::TestServer;
    use std::sync::{Arc, Mutex};

    type Sent = Arc<Mutex<Vec<OutgoingEmail>>>;
    async fn fake_send(State(sent): State<Sent>, Json(email): Json<OutgoingEmail>) -> Json<Value> {
        sent.lock().unwrap().push(email);
        Json(json!({ "message_id": "<m@test>" }))
    }
    make_super(&pool).await;
    let sent: Sent = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/email/send", post(fake_send))
        .with_state(sent.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool, common::SECRET, common::ALLOWED_ORIGIN).with_webhooks(
        WebhooksClient::new(Some(&format!("http://{address}")), Some("key")),
    );
    let server = TestServer::new(app(state));
    let body = json!({ "to": "a@example.com", "subject": "Hi", "text": "Line <1>\nLine 2" });
    server
        .post("/v1/superuser/emails")
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/superuser/emails")
        .with_session(sessions::SUPER_ADMIN)
        .json(&json!({ "to": "nope", "subject": "Hi", "text": "x" }))
        .await
        .assert_status_bad_request();
    server
        .post("/v1/superuser/emails")
        .with_session(sessions::SUPER_ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, ["a@example.com"]);
    assert_eq!(sent[0].body, "Line &lt;1&gt;<br>Line 2");
    assert!(sent[0].transactional);
}

#[test_db::test(seed)]
async fn super_admins_read_the_sales_mailbox(pool: MySqlPool) {
    make_super(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO emails (id, thread_id, subject, body, sender_email, receiver_email, sent_at, company_id, is_draft) VALUES
           (9900, 'sales-t1', 'Pricing?', 'How much?', 'Alice <alice@example.com>', 'sales@granite-manager.com', '2026-05-01 10:00:00', 100, 0),
           (9901, 'sales-t1', 'Re: Pricing?', 'Plans start at...', 'sales@granitemanager.com', 'alice@example.com', '2026-05-01 11:00:00', 100, 0),
           (9902, 'sales-t2', 'Demo', 'Can we talk', 'bob@example.com', 'SALES@granitemanager.com', '2026-05-02 10:00:00', NULL, 0),
           (9903, 'other-t', 'Hi', 'x', 'carl@example.com', 'rep@acme.test', '2026-05-03 10:00:00', 100, 0);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    server
        .get("/v1/superuser/sales-emails")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let inbox: Value = server
        .get("/v1/superuser/sales-emails")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(inbox["total_count"], 2);
    let threads: Vec<&str> = inbox["emails"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["thread_id"].as_str().unwrap())
        .collect();
    assert_eq!(threads, ["sales-t2", "sales-t1"]);
    assert_eq!(inbox["emails"][1]["sender_name"], "Alice Lead");
    let sent: Value = server
        .get("/v1/superuser/sales-emails?folder=sent")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(sent["emails"][0]["id"], 9901);
    let found: Value = server
        .get("/v1/superuser/sales-emails?search=demo")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(found["total_count"], 1);

    let thread: Value = server
        .get("/v1/superuser/sales-emails/sales-t1")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .json();
    assert_eq!(thread["messages"].as_array().unwrap().len(), 2);
    assert_eq!(thread["customer"]["name"], "Alice Lead");
    server
        .get("/v1/superuser/sales-emails/other-t")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status_not_found();
}
