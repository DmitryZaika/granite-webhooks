use crate::common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingEmail, OutgoingSms, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{Duration, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sent {
    texts: Arc<Mutex<Vec<OutgoingSms>>>,
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
}

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    sqlx::query("UPDATE users SET cloudtalk_agent_id = '5001' WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
    let sent = Sent::default();
    let router = Router::new()
        .route(
            "/sms/send",
            post(
                |State(sent): State<Sent>, Json(sms): Json<OutgoingSms>| async move {
                    sent.texts.lock().unwrap().push(sms);
                    Json(json!({ "ids": [] }))
                },
            ),
        )
        .route(
            "/email/send",
            post(
                |State(sent): State<Sent>, Json(email): Json<OutgoingEmail>| async move {
                    sent.emails.lock().unwrap().push(email);
                    Json(json!({ "message_id": "<m-1@ses>" }))
                },
            ),
        )
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("key"),
        ));
    (TestServer::new(app(state)), sent)
}

async fn status(pool: &MySqlPool, table: &str, id: i64) -> String {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {table} WHERE id = ?"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn a_scheduled_text_is_edited_and_sent_now(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let later = Utc::now() + Duration::hours(2);
    server
        .post("/v1/sms/cloudtalk/messages")
        .with_session(sessions::REP)
        .json(&json!({ "phone_digits": "3175550100", "text": "See you soon", "send_at": later }))
        .await
        .assert_status_ok();
    let list: Value = server
        .get("/v1/delayed/sms?phone_digits=3175550100")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["text"], "See you soon");
    let id = list[0]["id"].as_i64().unwrap();

    server
        .put(&format!("/v1/delayed/sms/{id}"))
        .with_session(sessions::REP)
        .json(&json!({ "text": "See you at 3", "send_at": later + Duration::hours(1) }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .put(&format!("/v1/delayed/sms/{id}"))
        .with_session(sessions::REP)
        .json(&json!({ "text": "x", "send_at": Utc::now() - Duration::hours(1) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Another user cannot touch it.
    server
        .delete(&format!("/v1/delayed/sms/{id}"))
        .with_session(sessions::MANAGER)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .post(&format!("/v1/delayed/sms/{id}/send-now"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let texts = sent.texts.lock().unwrap().clone();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        (texts[0].text.as_str(), texts[0].agent_id.as_str()),
        ("See you at 3", "5001")
    );
    assert_eq!(status(&pool, "delayed_sms", id).await, "sent");
}

#[test_db::test(seed)]
async fn a_reply_drops_a_waiting_email(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let later = Utc::now() + Duration::hours(2);
    server
        .post("/v1/emails")
        .with_session(sessions::REP)
        .json(&json!({
            "to": ["alice@example.com"], "subject": "Quote", "body": "<p>Here it is</p>",
            "customer_id": 1000, "send_at": later, "cancel_if_customer_messages": true,
        }))
        .await
        .assert_status_ok();
    let list: Value = server
        .get("/v1/delayed/emails?customer_id=1000")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(list[0]["text"], "Quote — <p>Here it is</p>");
    let id = list[0]["id"].as_i64().unwrap();

    // Alice writes back: the waiting email is dropped.
    sqlx::query(
        "INSERT INTO emails (subject, body, sender_email, sent_at, company_id, thread_id)
         VALUES ('Re', 'Thanks', 'Alice <alice@example.com>', UTC_TIMESTAMP() + INTERVAL 1 SECOND, 100, 'reply-thread')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let after: Value = server
        .get("/v1/delayed/emails?customer_id=1000")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(after, json!([]));
    assert_eq!(status(&pool, "delayed_emails", id).await, "cancelled");
    assert!(sent.emails.lock().unwrap().is_empty());
}

#[test_db::test(seed)]
async fn a_due_email_goes_out_when_the_list_is_read(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    sqlx::query(
        "INSERT INTO delayed_emails (company_id, user_id, customer_id, thread_id, recipient_emails, send_at,
            cancel_if_customer_messages, baseline_at, status, payload_json)
         VALUES (100, 100, 1000, NULL, '[\"alice@example.com\"]', UTC_TIMESTAMP() - INTERVAL 1 MINUTE, 0,
            UTC_TIMESTAMP() - INTERVAL 1 HOUR, 'pending',
            '{\"to\":[\"alice@example.com\"],\"subject\":\"Hello\",\"body\":\"<p>Hi Alice</p>\",\"customerId\":\"1000\"}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    server
        .get("/v1/delayed/emails?customer_id=1000")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "Hello");
    assert_eq!(emails[0].to, ["alice@example.com"]);

    // The sender hears it went out, on Alice's latest deal.
    let notice: (i64, Option<i64>, String, String) = sqlx::query_as(
        "SELECT CAST(user_id AS SIGNED), CAST(deal_id AS SIGNED), message, notification_type
           FROM notifications WHERE notification_type LIKE 'delayed_message_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let latest: i64 = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM deals WHERE customer_id = 1000 AND deleted_at IS NULL
          ORDER BY is_won IS NULL DESC, id DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notice,
        (
            100,
            Some(latest),
            "Scheduled email sent: \"Hello\"".into(),
            "delayed_message_sent".into()
        )
    );
}
