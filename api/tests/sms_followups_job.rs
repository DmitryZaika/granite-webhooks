//! `POST /v1/jobs/sms-followups`: lead-list drip texts and everyone's
//! scheduled messages (the flow steps it also sends: `followup_flows.rs`).

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
use chrono::{Duration, Utc};
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

// Acme (100) texts through CloudTalk; Rita (100) is agent 5001. Alice (1000)
// is a lead with deal 8000 in list 101; 8671 is a company template ("Hi").
// Drip rows: 9900 due, 9901 its deal moved to another list (left pending),
// 9902 a deleted template, 9903 a variable only the rep can fill, 9904 not
// due yet.
const FIXTURE: &str = "
UPDATE company SET cloudtalk_access_key = 'ct-key', cloudtalk_access_secret = 'ct-secret' WHERE id = 100;
UPDATE users SET cloudtalk_agent_id = '5001' WHERE id = 100;
UPDATE customers SET source = 'leads' WHERE id = 1000;
INSERT INTO cloudtalk_sms_templates (id, company_id, user_id, name, body, position, deleted_at) VALUES
  (9800, 100, NULL, 'Old', 'Old text', 5, '2026-09-01 00:00:00'),
  (9801, 100, NULL, 'Gate', 'Gate code {{gate code}}', 6, NULL),
  (9802, 100, NULL, 'Named', 'Hi {{customer.first_name}}, your quote is ready', 7, NULL);
INSERT INTO scheduled_sms (id, template_id, deal_id, list_id, customer_id, user_id, company_id, send_at) VALUES
  (9900, 9802, 8000, 101, 1000, 100, 100, '2026-10-01 09:00:00'),
  (9901, 8671, 8000, 100, 1000, 100, 100, '2026-10-01 09:00:00'),
  (9902, 9800, 8000, NULL, 1000, 100, 100, '2026-10-01 09:00:00'),
  (9903, 9801, 8000, NULL, 1000, 100, 100, '2026-10-01 09:00:00'),
  (9904, 8671, 8000, NULL, 1000, 100, 100, '2099-01-01 09:00:00');
";

#[derive(Clone)]
struct Fake {
    pool: MySqlPool,
    texts: Arc<Mutex<Vec<OutgoingSms>>>,
}

async fn send_text(State(fake): State<Fake>, Json(sms): Json<OutgoingSms>) -> Json<Value> {
    fake.texts.lock().unwrap().push(sms.clone());
    let id = sqlx::query(
        "INSERT INTO cloudtalk_sms (cloudtalk_id, recipient, text, direction, status, agent, sender_user_id, company_id)
         VALUES (777001, ?, ?, 'outbound', 'sent', ?, ?, ?)",
    )
    .bind(&sms.phone_digits)
    .bind(&sms.text)
    .bind(&sms.agent_id)
    .bind(sms.user_id)
    .bind(sms.company_id)
    .execute(&fake.pool)
    .await
    .unwrap()
    .last_insert_id();
    Json(json!({ "ids": [id] }))
}

async fn setup(pool: &MySqlPool) -> (TestServer, Arc<Mutex<Vec<OutgoingSms>>>) {
    sqlx::raw_sql(FIXTURE).execute(pool).await.unwrap();
    let texts = Arc::new(Mutex::new(Vec::new()));
    let fake = Fake {
        pool: pool.clone(),
        texts: Arc::clone(&texts),
    };
    let router = Router::new()
        .route("/sms/send", post(send_text))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_storage(Storage::memory())
        .with_webhooks(WebhooksClient::new(Some(&base), Some("webhooks-key")))
        .with_job_key("job-secret");
    (TestServer::new(app(state)), texts)
}

async fn run(server: &TestServer) -> Value {
    let res = server
        .post("/v1/jobs/sms-followups")
        .add_header("Authorization", "job-secret")
        .await;
    res.assert_status_ok();
    res.json()
}

async fn drip(pool: &MySqlPool, id: i32) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT CAST(status AS CHAR), error_message, provider_message_id FROM scheduled_sms WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[test_db::test(seed)]
async fn due_drip_texts_go_out_or_fail_with_a_reason(pool: MySqlPool) {
    let (server, texts) = setup(&pool).await;

    let body = run(&server).await;
    assert_eq!(
        body["scheduled_sms"],
        json!({ "processed": 3, "sent": 1, "failed": 2 })
    );
    let texts = texts.lock().unwrap().clone();
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0].text, "Hi Alice, your quote is ready");
    assert_eq!(texts[0].phone_digits, "3175550100");
    assert_eq!(
        (texts[0].agent_id.as_str(), texts[0].user_id),
        ("5001", 100)
    );
    assert!(texts[0].is_automated);
    assert_eq!(texts[0].idempotency_key.as_deref(), Some("sms-drip-9900"));

    assert_eq!(
        drip(&pool, 9900).await,
        ("sent".into(), None, Some("777001".into()))
    );
    assert_eq!(drip(&pool, 9901).await, ("pending".into(), None, None));
    assert_eq!(
        drip(&pool, 9902).await,
        ("failed".into(), Some("Template deleted".into()), None)
    );
    assert_eq!(
        drip(&pool, 9903).await,
        (
            "failed".into(),
            Some("Unfilled template variables".into()),
            None
        )
    );
    assert_eq!(drip(&pool, 9904).await, ("pending".into(), None, None));

    // Nothing is sent twice.
    let body = run(&server).await;
    assert_eq!(body["scheduled_sms"]["sent"], 0);
}

#[test_db::test(seed)]
async fn a_lead_without_a_phone_or_a_rep_without_a_line_fails_the_text(pool: MySqlPool) {
    let (server, texts) = setup(&pool).await;
    sqlx::query("UPDATE scheduled_sms SET status = 'cancelled' WHERE id IN (9901, 9902, 9903)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET cloudtalk_agent_id = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    run(&server).await;
    assert_eq!(
        drip(&pool, 9900).await,
        (
            "failed".into(),
            Some("Sales rep is not linked".into()),
            None
        )
    );

    sqlx::query(
        "UPDATE scheduled_sms SET status = 'pending', error_message = NULL WHERE id = 9900",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE customers SET phone = NULL, phone_2 = '' WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    run(&server).await;
    assert_eq!(
        drip(&pool, 9900).await,
        (
            "failed".into(),
            Some("Customer has no phone number".into()),
            None
        )
    );
    assert!(texts.lock().unwrap().is_empty());
}

#[test_db::test(seed)]
async fn everyones_scheduled_texts_go_out(pool: MySqlPool) {
    let (server, texts) = setup(&pool).await;
    sqlx::query("UPDATE scheduled_sms SET status = 'cancelled'")
        .execute(&pool)
        .await
        .unwrap();
    let later = Utc::now() + Duration::hours(2);
    server
        .post("/v1/sms/cloudtalk/messages")
        .with_session(sessions::REP)
        .json(&json!({ "phone_digits": "3175550100", "text": "See you soon", "send_at": later }))
        .await
        .assert_status_ok();
    sqlx::query("UPDATE delayed_sms SET send_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE")
        .execute(&pool)
        .await
        .unwrap();

    let body = run(&server).await;
    assert_eq!(body["delayed"], json!({ "emails": 0, "sms": 1 }));
    assert_eq!(texts.lock().unwrap()[0].text, "See you soon");
    let status: String = sqlx::query_scalar("SELECT CAST(status AS CHAR) FROM delayed_sms")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "sent");
}

#[test_db::test(seed)]
async fn the_job_needs_its_key(pool: MySqlPool) {
    let (server, _) = setup(&pool).await;
    server
        .post("/v1/jobs/sms-followups")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .post("/v1/jobs/sms-followups")
        .add_header("Authorization", "wrong")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
