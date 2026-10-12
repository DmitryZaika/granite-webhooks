mod common;

use api::app;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, TelegramCrmNotify, WebhooksClient};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Sent {
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
    telegrams: Arc<Mutex<Vec<TelegramCrmNotify>>>,
}

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    let sent = Sent::default();
    let router = Router::new()
        .route(
            "/email/send",
            post(
                |State(sent): State<Sent>, Json(email): Json<OutgoingEmail>| async move {
                    sent.emails.lock().unwrap().push(email);
                    Json(json!({ "message_id": "m-1" }))
                },
            ),
        )
        .route(
            "/telegram/crm-notify",
            post(
                |State(sent): State<Sent>, Json(notify): Json<TelegramCrmNotify>| async move {
                    sent.telegrams.lock().unwrap().push(notify);
                    Json(json!({}))
                },
            ),
        )
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(
            Some(&format!("http://{address}")),
            Some("key"),
        ))
        .with_job_key("job-secret");
    (TestServer::new(app(state)), sent)
}

#[test_db::test(seed)]
async fn due_appointment_reminders_are_sent_once(pool: MySqlPool) {
    sqlx::raw_sql("INSERT INTO calendars (id, company_id, name, color, slug) VALUES (8820, 100, 'Estimates', 'blue', 'estimates');")
        .execute(&pool)
        .await
        .unwrap();
    let (server, sent) = server(&pool).await;
    let booked: Value = server
        .post("/v1/events/estimates")
        .with_session(sessions::REP)
        .json(&json!({
            "customer_id": 1000, "calendar_id": 8820, "title": "1000",
            "start_date": "2099-01-15 15:00:00", "end_date": "2099-01-15 16:00:00", "location": "1 Main St",
        }))
        .await
        .json();
    let event = booked["id"].as_i64().unwrap();
    sent.emails.lock().unwrap().clear();
    sqlx::query("UPDATE appointment_reminders SET send_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE event_id = ? AND reminder_kind = '24h'")
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE appointment_rep_reminders SET send_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE event_id = ? AND reminder_kind = '1h'")
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    server
        .post("/v1/jobs/appointment-reminders")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    let run = || {
        server
            .post("/v1/jobs/appointment-reminders")
            .add_header("Authorization", "job-secret")
    };
    let body: Value = run().await.json();
    assert_eq!(body["processed"], 1);
    assert_eq!(body["sent"], 1);
    assert_eq!(body["rep_processed"], 1);
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "Acme Stone - In-Home Estimate Reminder");
    assert!(
        emails[0]
            .body
            .starts_with("<p>Hi Alice, this is a reminder that your in-home estimate with Rita")
    );
    let rep_sent = body["rep_sent"].as_u64().unwrap();
    if rep_sent == 1 {
        let telegrams = sent.telegrams.lock().unwrap().clone();
        assert_eq!(
            telegrams[0].notification_type,
            "estimate_appointment_reminder"
        );
        assert!(
            telegrams[0]
                .message
                .starts_with("In-home estimate in 1 hour with Alice Lead at ")
        );
    }
    let again: Value = run().await.json();
    assert_eq!(again["processed"], 0);
    assert_eq!(again["rep_processed"], 0);
}
