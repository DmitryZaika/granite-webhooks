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

#[test_db::test(seed)]
async fn shop_workers_get_one_maintenance_digest_a_day(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE users SET notifications_telegram_id = 555 WHERE id = 100;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 4, 100);
         INSERT INTO maintenance_items (id, company_id, name, start_date, created_by_user_id) VALUES
           (9601, 100, 'Saw', '2026-01-01', 102), (9602, 101, 'Other', '2026-01-01', 106);
         INSERT INTO maintenance_sub_items (id, company_id, maintenance_item_id, name, created_by_user_id) VALUES
           (9611, 100, 9601, 'Blade', 102), (9612, 100, 9601, ' ', 102), (9613, 101, 9602, 'Belt', 106);
         INSERT INTO maintenance_conditions (company_id, maintenance_sub_item_id, condition_type, interval_value, next_due_date) VALUES
           (100, 9611, 'days', 7, '2026-06-01'), (100, 9611, 'hours', 40, '2026-06-01'),
           (100, 9612, 'days', 7, '2026-06-01'), (101, 9613, 'days', 7, '2026-06-01'),
           (100, 9611, 'days', 7, '2026-06-02');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let telegrams = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = Arc::clone(&telegrams);
    let router = Router::new().route(
        "/telegram/notifications-notify",
        post(move |Json(body): Json<Value>| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock().unwrap().push(body);
                Json(json!({}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_webhooks(
        WebhooksClient::new(Some(&format!("http://{address}")), Some("key")),
    );
    let day = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    let run = api::routes::jobs::maintenance_reminders::send_digests(&state, day)
        .await
        .unwrap();
    assert_eq!((run.processed, run.sent, run.failed), (1, 1, 0));
    let sent = telegrams.lock().unwrap().clone();
    assert_eq!(sent[0]["user_id"], 100);
    let message = sent[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with("🔧 Maintenance due today\n\n• Saw · Check\n• Saw · Blade\n\nOpen: ")
    );
    let again = api::routes::jobs::maintenance_reminders::send_digests(&state, day)
        .await
        .unwrap();
    assert_eq!((again.processed, again.sent), (1, 0));
    assert_eq!(telegrams.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn the_survey_goes_out_once_then_reminds(pool: MySqlPool) {
    // Alice's checklist (seed 9000) was signed 30 hours ago.
    sqlx::raw_sql(
        "DELETE FROM emails WHERE message_id LIKE 'checklist-survey-%';
         UPDATE checklists SET created_at = UTC_TIMESTAMP() - INTERVAL 30 HOUR WHERE id = 9000;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let sent = Sent::default();
    let address = sent_address(&sent).await;
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&address), Some("key")));
    let survey = api::routes::jobs::survey_notifications::send_due;
    let run = survey(&state, chrono::Utc::now()).await.unwrap();
    assert_eq!((run.initial.sent, run.reminder1.processed), (1, 0));
    {
        let emails = sent.emails.lock().unwrap();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].to, ["alice@example.test"]);
        assert_eq!(emails[0].subject, "Acme Stone - Post-Installation Survey");
        assert!(
            emails[0]
                .body
                .contains("/customer/100/survey?c=9000&amp;t=")
        );
    }
    let again = survey(&state, chrono::Utc::now()).await.unwrap();
    assert_eq!((again.initial.sent, again.initial.skipped), (0, 1));

    // Three and a half days on, the first reminder (by email: no CloudTalk).
    sqlx::raw_sql(
        "UPDATE checklists SET created_at = UTC_TIMESTAMP() - INTERVAL 84 HOUR WHERE id = 9000;
         UPDATE emails SET sent_at = UTC_TIMESTAMP() - INTERVAL 2 DAY WHERE message_id = 'checklist-survey-9000';",
    )
    .execute(&pool)
    .await
    .unwrap();
    let reminded = survey(&state, chrono::Utc::now()).await.unwrap();
    assert_eq!(reminded.reminder1.sent, 1);
    let emails = sent.emails.lock().unwrap();
    assert_eq!(
        emails[1].subject,
        "Acme Stone - Reminder: Post-Installation Survey"
    );
}

/// A webhooks stub recording into `sent`; its base URL.
async fn sent_address(sent: &Sent) -> String {
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
        .with_state(sent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}
