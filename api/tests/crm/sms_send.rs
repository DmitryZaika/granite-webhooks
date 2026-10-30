use crate::common;

use api::app;
use api::state::AppState;
use api::storage::Storage;
use api::webhooks_client::{OutgoingSms, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

// Rita (100) texts through CloudTalk as agent 5001; template 8671 is a
// company SMS template. The fake webhooks Lambda stores a sent row the way
// the real one does.

#[derive(Clone)]
enum Reply {
    Sent,
    Failed(Value),
}

#[derive(Clone)]
struct Fake {
    pool: MySqlPool,
    seen: Arc<Mutex<Vec<OutgoingSms>>>,
    reply: Reply,
}

async fn fake_send(
    State(fake): State<Fake>,
    headers: axum::http::HeaderMap,
    Json(sms): Json<OutgoingSms>,
) -> (StatusCode, Json<Value>) {
    assert_eq!(headers["authorization"], "webhooks-key");
    fake.seen.lock().unwrap().push(sms.clone());
    match fake.reply {
        Reply::Sent => {
            let id = sqlx::query(
                "INSERT INTO cloudtalk_sms (recipient, text, direction, status, agent, sender_user_id,
                                            company_id, recipient10, phone_digits)
                 VALUES (?, ?, 'outbound', 'sent', ?, ?, ?, ?, ?)",
            )
            .bind(&sms.phone_digits)
            .bind(&sms.text)
            .bind(&sms.agent_id)
            .bind(sms.user_id)
            .bind(sms.company_id)
            .bind(&sms.phone_digits)
            .bind(&sms.phone_digits)
            .execute(&fake.pool)
            .await
            .unwrap()
            .last_insert_id();
            (StatusCode::OK, Json(json!({ "ids": [id] })))
        }
        Reply::Failed(body) => (StatusCode::BAD_GATEWAY, Json(body)),
    }
}

struct Setup {
    server: TestServer,
    seen: Arc<Mutex<Vec<OutgoingSms>>>,
}

async fn setup(pool: &MySqlPool, reply: Reply) -> Setup {
    sqlx::query("UPDATE users SET cloudtalk_agent_id = '5001' WHERE id = 100")
        .execute(pool)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        pool: pool.clone(),
        seen: Arc::clone(&seen),
        reply,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/sms/send", post(fake_send))
        .with_state(fake);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("webhooks-key"),
        ));
    Setup {
        server: TestServer::new(app(state)),
        seen,
    }
}

impl Setup {
    async fn send(&self, session: &str, body: Value) -> axum_test::TestResponse {
        self.server
            .post("/v1/sms/cloudtalk/messages")
            .with_session(session)
            .json(&body)
            .await
    }

    fn seen(&self) -> Vec<OutgoingSms> {
        self.seen.lock().unwrap().clone()
    }
}

#[test_db::test(seed)]
async fn a_text_goes_out_and_marks_the_thread_read(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let res = setup
        .send(
            sessions::REP,
            json!({
                "phone_digits": "3175550100",
                "text": "  Your quote  ",
                "template_id": 8671,
                "attachments": [{ "s3_key": "sms-attachments/100/m/a.jpg", "content_type": "image/jpeg",
                                  "filename": "a.jpg", "width": 4, "height": 3 }],
            }),
        )
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["delayed"], false);
    assert_eq!(body["message"]["text"], "Your quote");
    assert_eq!(body["message"]["direction"], "outbound");
    assert_eq!(body["message"]["agent_name"], "Rita Rep");

    let sent = &setup.seen()[0];
    assert_eq!(sent.provider, "cloudtalk");
    assert_eq!((sent.company_id, sent.user_id), (100, 100));
    assert_eq!(sent.agent_id, "5001");
    assert_eq!(sent.text, "Your quote");
    assert!(
        sent.attachments[0]
            .url
            .ends_with("/sms-attachments/100/m/a.jpg")
    );
    assert!(!sent.is_automated);

    let read: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cloudtalk_sms_thread_reads WHERE user_id = 100 AND customer_phone_digits = '3175550100'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(read, 1);
    let used: i64 = sqlx::query_scalar(
        "SELECT usage_count FROM user_template_usage WHERE user_id = 100 AND template_type = 'cloudtalk_sms' AND template_id = 8671",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(used, 1);
}

#[test_db::test(seed)]
async fn bad_texts_are_refused_before_sending(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let refused = |body: Value| async { setup.send(sessions::REP, body).await };
    for (body, token) in [
        (
            json!({ "phone_digits": "3175550100", "text": " " }),
            "empty_message",
        ),
        (
            json!({ "phone_digits": "555", "text": "hi" }),
            "invalid_phone",
        ),
        (
            json!({ "phone_digits": "3175550100", "text": "x".repeat(1601) }),
            "text_too_long",
        ),
        (
            json!({ "phone_digits": "3175550100", "text": "hi",
                    "attachments": [{ "s3_key": "sms-attachments/101/m/a.jpg", "content_type": "image/jpeg" }] }),
            "invalid_attachment",
        ),
    ] {
        let res = refused(body).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(res.json::<Value>()["error"], token);
    }
    // Mark has no agent id.
    let res = setup
        .send(
            sessions::MANAGER,
            json!({ "phone_digits": "3175550100", "text": "hi" }),
        )
        .await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(res.json::<Value>()["error"], "agent_not_linked");
    assert!(setup.seen().is_empty());
}

#[test_db::test(seed)]
async fn provider_failures_come_back_as_tokens(pool: MySqlPool) {
    let setup = setup(
        &pool,
        Reply::Failed(
            json!({ "error": "ringcentral_mms_not_supported", "ids": [5],
                              "unsent_s3_keys": ["sms-attachments/100/m/b.jpg"] }),
        ),
    )
    .await;
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "hi" }),
        )
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        res.json::<Value>(),
        json!({ "error": "ringcentral_mms_not_supported", "sent_messages": 1,
                "unsent_s3_keys": ["sms-attachments/100/m/b.jpg"] })
    );
}

#[test_db::test(seed)]
async fn a_scheduled_text_waits_in_delayed_sms(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let at = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "Later", "send_at": at,
                    "customer_id": 1000, "cancel_if_customer_messages": true, "template_id": 8671 }),
        )
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "message": null, "delayed": true })
    );
    let (provider, customer, cancel, payload): (String, Option<i32>, i8, String) = sqlx::query_as(
        "SELECT CAST(provider AS CHAR), customer_id, cancel_if_customer_messages, payload_json FROM delayed_sms",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (provider.as_str(), customer, cancel),
        ("cloudtalk", Some(1000), 1)
    );
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap(),
        json!({ "phoneDigits": "3175550100", "text": "Later", "attachments": null, "templateId": "8671" })
    );
    assert!(setup.seen().is_empty());

    let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "Late", "send_at": past }),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn ten_texts_a_minute_at_most(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    for _ in 0..10 {
        setup
            .send(
                sessions::REP,
                json!({ "phone_digits": "3175550100", "text": "hi" }),
            )
            .await
            .assert_status_ok();
    }
    let res = setup
        .send(
            sessions::REP,
            json!({ "phone_digits": "3175550100", "text": "hi" }),
        )
        .await;
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.json::<Value>()["error"], "rate_limited");
}

/// A failed text of `user_id`'s to 3175550100 with one picture; its id.
async fn failed_text(pool: &MySqlPool, user_id: i32) -> u64 {
    let id = sqlx::query(
        "INSERT INTO cloudtalk_sms (recipient, text, direction, status, error_message, agent,
                                    sender_user_id, company_id, recipient10, phone_digits, created_date)
         VALUES (3175550100, 'Your quote', 'outbound', 'failed', 'cloudtalk_403', '5001', ?, 100,
                 '3175550100', '3175550100', NOW() - INTERVAL 1 HOUR)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    sqlx::query(
        "INSERT INTO cloudtalk_sms_attachments (cloudtalk_sms_id, content_type, filename, s3_key, s3_url, width, height)
         VALUES (?, 'image/jpeg', 'a.jpg', 'sms-attachments/100/m/a.jpg', 'https://x/a.jpg', 4, 3)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn status_of(pool: &MySqlPool, id: u64) -> Option<String> {
    sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM cloudtalk_sms WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn a_retried_text_replaces_the_failed_one(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let failed = failed_text(&pool, 100).await;
    let res = setup
        .server
        .post(&format!("/v1/sms/cloudtalk/messages/{failed}/retry"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["message"]["text"], "Your quote");
    assert_eq!(body["message"]["status"], "sent");

    let sent = &setup.seen()[0];
    assert_eq!(
        (sent.phone_digits.as_str(), sent.text.as_str()),
        ("3175550100", "Your quote")
    );
    assert_eq!(sent.attachments.len(), 1);
    assert!(
        sent.attachments[0]
            .url
            .ends_with("/sms-attachments/100/m/a.jpg")
    );
    assert_eq!(status_of(&pool, failed).await, None);
}

#[test_db::test(seed)]
async fn only_the_senders_failed_texts_can_be_retried(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Sent).await;
    let theirs = failed_text(&pool, 101).await;
    let mine = failed_text(&pool, 100).await;
    sqlx::query("UPDATE cloudtalk_sms SET status = 'sent' WHERE id = ?")
        .bind(mine)
        .execute(&pool)
        .await
        .unwrap();
    for (session, id) in [
        (sessions::REP, theirs),
        (sessions::REP, mine),
        (sessions::OTHER_COMPANY_REP, theirs),
    ] {
        setup
            .server
            .post(&format!("/v1/sms/cloudtalk/messages/{id}/retry"))
            .with_session(session)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
    assert!(setup.seen().is_empty());
    assert_eq!(status_of(&pool, theirs).await.as_deref(), Some("failed"));
}

#[test_db::test(seed)]
async fn a_retry_that_fails_again_stays_failed(pool: MySqlPool) {
    let setup = setup(&pool, Reply::Failed(json!({ "error": "ringcentral_403" }))).await;
    let failed = failed_text(&pool, 100).await;
    let res = setup
        .server
        .post(&format!("/v1/sms/cloudtalk/messages/{failed}/retry"))
        .with_session(sessions::REP)
        .await;
    res.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(res.json::<Value>()["error"], "ringcentral_403");
    assert_eq!(status_of(&pool, failed).await.as_deref(), Some("failed"));
}
