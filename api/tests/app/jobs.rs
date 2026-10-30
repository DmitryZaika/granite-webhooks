use crate::common;

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
async fn shop_workers_and_managers_get_one_maintenance_digest_a_day(pool: MySqlPool) {
    // 100 is a shop worker and shop manager, 101 a shop manager.
    sqlx::raw_sql(
        "UPDATE users SET notifications_telegram_id = 555 WHERE id = 100;
         UPDATE users SET notifications_telegram_id = 556 WHERE id = 101;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (100, 4, 100), (100, 3, 100), (101, 3, 100);
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
    assert_eq!((run.processed, run.sent, run.failed), (2, 2, 0));
    let sent = telegrams.lock().unwrap().clone();
    assert_eq!(sent[0]["user_id"], 100);
    assert_eq!(sent[1]["user_id"], 101);
    for telegram in &sent {
        let message = telegram["message"].as_str().unwrap();
        assert!(
            message
                .starts_with("🔧 Maintenance due today\n\n• Saw · Check\n• Saw · Blade\n\nOpen: ")
        );
    }
    let again = api::routes::jobs::maintenance_reminders::send_digests(&state, day)
        .await
        .unwrap();
    assert_eq!((again.processed, again.sent), (2, 0));
    assert_eq!(telegrams.lock().unwrap().len(), 2);
}

#[test_db::test(seed)]
async fn the_survey_goes_out_once_then_reminds(pool: MySqlPool) {
    // Alice's checklist (seed 9000) was signed 30 hours ago.
    sqlx::raw_sql(
        "DELETE FROM emails WHERE message_id LIKE 'checklist-survey-%';
         UPDATE checklists SET created_at = UTC_TIMESTAMP() - INTERVAL 30 HOUR, installer_id = 101
          WHERE id = 9000;",
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
        // The checklist's installer rides along so the form picks them.
        assert!(emails[0].body.contains("&amp;i=101"));
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

#[test_db::test(seed)]
async fn outbound_calls_become_call_points(pool: MySqlPool) {
    // Olga (106, company 101 with CloudTalk keys) is agent 5106. A Granite
    // Phone call of hers waits for its transcript: the phone job scores it,
    // never this one.
    sqlx::raw_sql(
        "UPDATE users SET cloudtalk_agent_id = '5106' WHERE id = 106; DELETE FROM phone_call_points;
         INSERT INTO phone_call_points (company_id, provider, provider_call_id, user_id, customer_digits,
                                        started_at, talk_seconds, outcome)
         VALUES (101, 'telnyx', '981999', 106, '3175550100', '2026-05-01 09:00:00', 120, 'pending');",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = Router::new()
        .route(
            "/calls/index.json",
            axum::routing::get(|| async {
                Json(json!({ "responseData": { "data": [
                    { "Cdr": { "id": "1", "type": "outgoing", "user_id": "5106", "public_external": "+13175550100",
                               "started_at": "2026-05-01 10:00:00", "talking_time": "5" } },
                    { "Cdr": { "id": "2", "type": "outgoing", "user_id": "5106", "public_external": "+13175550100",
                               "started_at": "2026-05-01 11:00:00", "talking_time": "95" } },
                    { "Cdr": { "id": "3", "type": "outgoing", "user_id": "9999", "public_external": "+13175550100",
                               "started_at": "2026-05-01 12:00:00", "talking_time": "95" } },
                ] } }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&format!("http://{address}"))
        .with_job_key("job-secret");
    let server = TestServer::new(app(state));
    let body: Value = server
        .post("/v1/jobs/call-points")
        .add_header("Authorization", "job-secret")
        .await
        .json();
    assert_eq!(body["processed"], 2);
    // The long call has no recording: it scores as an attempt once classified.
    assert_eq!(body["classified"], 1);
    let rows: Vec<(String, i32, String)> = sqlx::query_as(
        "SELECT provider_call_id, user_id, CAST(outcome AS CHAR) FROM phone_call_points ORDER BY provider_call_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("1".into(), 106, "attempt".into()),
            ("2".into(), 106, "attempt".into()),
            ("981999".into(), 106, "pending".into())
        ]
    );
    let untouched: (i64, bool) = sqlx::query_as(
        "SELECT CAST(transcribe_attempts AS SIGNED), transcribe_claimed_at IS NULL
           FROM phone_call_points WHERE provider = 'telnyx'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(untouched, (0, true));
}

#[test_db::test(seed)]
async fn provider_calls_are_saved_for_the_history(pool: MySqlPool) {
    // Company 101 has CloudTalk keys.
    let router = Router::new().route(
        "/calls/index.json",
        axum::routing::get(|| async {
            Json(json!({ "responseData": { "data": [
                { "Cdr": { "id": 1, "type": "incoming", "public_external": "+13175550100",
                           "started_at": "2026-05-01 10:00:00" } },
                { "Cdr": { "id": "2", "type": "outgoing", "public_external": "+13175550101",
                           "started_at": "2026-05-01 11:00:00" } },
                { "Cdr": { "id": "3", "type": "internal", "public_external": "",
                           "started_at": "2026-05-01 12:00:00" } },
            ] } }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_cloudtalk_base(&format!("http://{address}"))
        .with_job_key("job-secret");
    let server = TestServer::new(app(state));
    for _ in 0..2 {
        let body: Value = server
            .post("/v1/jobs/call-history")
            .add_header("Authorization", "job-secret")
            .await
            .json();
        assert!(body["processed"].as_u64().unwrap() >= 2);
    }
    let rows: Vec<(i32, String, String, String)> = sqlx::query_as(
        "SELECT company_id, CAST(provider AS CHAR), call_id, customer_digits FROM provider_calls ORDER BY call_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (101, "cloudtalk".into(), "1".into(), "3175550100".into()),
            (101, "cloudtalk".into(), "2".into(), "3175550101".into()),
        ]
    );
    let synced: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_call_syncs WHERE company_id = 101 AND provider = 'cloudtalk'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(synced, 1);
}
