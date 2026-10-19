mod common;

use api::app;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, OutgoingSms, WebhooksClient};
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
    texts: Arc<Mutex<Vec<OutgoingSms>>>,
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
            "/sms/send",
            post(
                |State(sent): State<Sent>, Json(sms): Json<OutgoingSms>| async move {
                    sent.texts.lock().unwrap().push(sms);
                    Json(json!({ "ids": [1] }))
                },
            ),
        )
        .route("/telegram/crm-notify", post(|| async { Json(json!({})) }))
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

const CALENDARS: &str = "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
    (8830, 100, 'Installations', 'red', 'installations'),
    (8831, 100, 'Estimates', 'blue', 'estimates'),
    (8832, 101, 'Theirs', 'gray', NULL);";

const TEXTING: &str = "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100;
    UPDATE users SET cloudtalk_agent_id = '5101', cloudtalk_phone_number = '3175550999' WHERE id = 101;
    INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 10, 100);";

fn email_rule() -> Value {
    json!({
        "calendar_id": 8830, "channel": "email", "offset_value": 1, "offset_unit": "days",
        "send_time": "09:00", "subject": "Your installation on {{appointment.date}}",
        "body": "Hi {{customer.first_name}}, please disconnect the sink 2 hours before.",
    })
}

#[test_db::test(seed)]
async fn admins_manage_notification_rules(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let (server, _) = server(&pool).await;
    let list = || {
        server
            .get("/v1/schedule/notifications")
            .with_session(sessions::ADMIN)
    };
    server
        .get("/v1/schedule/notifications")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .post("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .json(&email_rule())
        .await
        .assert_status(StatusCode::CREATED);
    let mut sms = email_rule();
    sms["channel"] = json!("sms");
    let refused = server
        .post("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .json(&sms)
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Texting is not set up for this company"
    );
    for (field, value) in [
        ("calendar_id", json!(8832)),
        ("offset_value", json!(0)),
        ("offset_value", json!(31)),
        ("subject", json!(" ")),
        ("body", json!("")),
        ("send_time", json!("9am")),
    ] {
        let mut body = email_rule();
        body[field] = value;
        server
            .post("/v1/schedule/notifications")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let mut hours = email_rule();
    hours["offset_unit"] = json!("hours");
    server
        .post("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .json(&hours)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let body: Value = list().await.json();
    assert_eq!(body["sms_enabled"], false);
    let rule = &body["rules"][0];
    assert_eq!(body["rules"].as_array().unwrap().len(), 1);
    assert_eq!(
        (
            rule["channel"].as_str(),
            rule["offset_unit"].as_str(),
            rule["send_time"].as_str(),
            rule["is_active"].as_bool()
        ),
        (Some("email"), Some("days"), Some("09:00"), Some(true))
    );
    let id = rule["id"].as_i64().unwrap();

    // With texting set up a text rule is allowed; without it, it is hidden.
    sqlx::raw_sql(TEXTING).execute(&pool).await.unwrap();
    sms["send_time"] = json!(null);
    server
        .post("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .json(&sms)
        .await
        .assert_status(StatusCode::CREATED);
    let body: Value = list().await.json();
    assert_eq!(body["sms_enabled"], true);
    assert_eq!(body["rules"][1]["subject"], Value::Null);
    sqlx::query("UPDATE company SET cloudtalk_access_key = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let body: Value = list().await.json();
    assert_eq!(body["rules"].as_array().unwrap().len(), 1);

    let mut edited = email_rule();
    edited["offset_value"] = json!(2);
    edited["is_active"] = json!(false);
    server
        .put(&format!("/v1/schedule/notifications/{id}"))
        .with_session(sessions::ADMIN)
        .json(&edited)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body: Value = list().await.json();
    assert_eq!(body["rules"][0]["offset_value"], 2);
    assert_eq!(body["rules"][0]["is_active"], false);
    server
        .put(&format!("/v1/schedule/notifications/{id}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&edited)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .delete(&format!("/v1/schedule/notifications/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .delete(&format!("/v1/schedule/notifications/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let body: Value = list().await.json();
    assert_eq!(body["rules"].as_array().unwrap().len(), 0);
}

async fn rule(pool: &MySqlPool, calendar: i32, channel: &str, body: &str) {
    sqlx::query(
        "INSERT INTO schedule_notification_rules
           (company_id, calendar_id, channel, offset_value, offset_unit, subject, body, created_at)
         VALUES (100, ?, ?, 1, 'days', IF(? = 'email', 'Reminder for {{customer.first_name}}', NULL), ?, '2000-01-01')",
    )
    .bind(calendar)
    .bind(channel)
    .bind(channel)
    .bind(body)
    .execute(pool)
    .await
    .unwrap();
}

/// An event `hours` from now, made long ago (or just now).
async fn event(pool: &MySqlPool, calendar: i32, title: &str, hours: i32, made_now: bool) {
    sqlx::query(
        "INSERT INTO events (title, start_date, end_date, created_user_id, calendar_id, location, created_date)
         VALUES (?, UTC_TIMESTAMP() + INTERVAL ? HOUR, UTC_TIMESTAMP() + INTERVAL ? HOUR, 100, ?, '1 Main St',
                 IF(?, CURRENT_TIMESTAMP, '2000-01-01'))",
    )
    .bind(title)
    .bind(hours)
    .bind(hours + 2)
    .bind(calendar)
    .bind(made_now)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn due_rules_email_the_customer_once(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    rule(
        &pool,
        8830,
        "email",
        "Hi {{customer.first_name}}, your installation is {{appointment.date}} at {{appointment.address}}.\n\nPlease disconnect the sink 2 hours before.",
    )
    .await;
    // Due: tomorrow minus a day is now.
    event(&pool, 8830, "1000", 23, false).await;
    // Not due yet.
    event(&pool, 8830, "1000", 30, false).await;
    // No customer, and booked after its send time: skipped.
    event(&pool, 8830, "Crew meeting", 23, false).await;
    event(&pool, 8830, "1000", 22, true).await;
    let (server, sent) = server(&pool).await;
    let run = || {
        server
            .post("/v1/jobs/appointment-reminders")
            .add_header("Authorization", "job-secret")
    };
    let body: Value = run().await.json();
    assert_eq!(
        (&body["rule_processed"], &body["rule_sent"], &body["rule_failed"]),
        (&json!(1), &json!(1), &json!(0))
    );
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "Reminder for Alice");
    assert!(emails[0].body.starts_with("<p>Hi Alice, your installation is "));
    assert!(
        emails[0]
            .body
            .ends_with(" at 1 Main St.</p><p>Please disconnect the sink 2 hours before.</p>")
    );
    let statuses: Vec<String> = sqlx::query_scalar(
        "SELECT CONCAT(status, ':', COALESCE(error_message, '')) FROM schedule_notification_sends ORDER BY status, error_message",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        statuses,
        vec![
            "sent:",
            "skipped:booked_after_send_time",
            "skipped:no_customer"
        ]
    );
    let again: Value = run().await.json();
    assert_eq!(again["rule_processed"], 0);
    assert_eq!(sent.emails.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn text_rules_go_from_the_office_manager(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    rule(
        &pool,
        8830,
        "sms",
        "Hi {{customer.first_name}}, see you {{appointment.date}} at {{appointment.time}}.",
    )
    .await;
    event(&pool, 8830, "1000", 23, false).await;
    let (server, sent) = server(&pool).await;
    let run = || {
        server
            .post("/v1/jobs/appointment-reminders")
            .add_header("Authorization", "job-secret")
    };
    // Without texting the text fails rather than going out by email.
    let body: Value = run().await.json();
    assert_eq!(body["rule_failed"], 1);
    assert!(sent.emails.lock().unwrap().is_empty());
    sqlx::raw_sql(TEXTING).execute(&pool).await.unwrap();
    event(&pool, 8830, "1000", 23, false).await;
    let body: Value = run().await.json();
    assert_eq!(body["rule_sent"], 1);
    let texts = sent.texts.lock().unwrap().clone();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        (
            texts[0].provider.as_str(),
            texts[0].user_id,
            texts[0].phone_digits.as_str()
        ),
        ("cloudtalk", 101, "3175550100")
    );
    assert!(texts[0].text.starts_with("Hi Alice, see you "));
    assert!(!texts[0].text.contains("{{"));

    // RingCentral too, but the office manager only has a CloudTalk line.
    sqlx::query(
        "UPDATE company SET ringcentral_client_id = 'i', ringcentral_client_secret = 's', ringcentral_jwt = 'j' WHERE id = 100",
    )
    .execute(&pool)
    .await
    .unwrap();
    event(&pool, 8830, "1000", 23, false).await;
    let body: Value = run().await.json();
    assert_eq!(body["rule_sent"], 1);
    let texts = sent.texts.lock().unwrap().clone();
    assert_eq!(texts.len(), 2);
    assert_eq!(texts[1].provider, "cloudtalk");
}

#[test_db::test(seed)]
async fn a_rule_replaces_the_built_in_reminders(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let (server, sent) = server(&pool).await;
    let booked: Value = server
        .post("/v1/events/estimates")
        .with_session(sessions::REP)
        .json(&json!({
            "customer_id": 1000, "calendar_id": 8831, "title": "1000",
            "start_date": "2099-01-15 15:00:00", "end_date": "2099-01-15 16:00:00",
        }))
        .await
        .json();
    let event = booked["id"].as_i64().unwrap();
    sent.emails.lock().unwrap().clear();
    rule(&pool, 8831, "email", "Hi {{customer.first_name}}").await;
    sqlx::query("UPDATE appointment_reminders SET send_at = UTC_TIMESTAMP() - INTERVAL 1 MINUTE WHERE event_id = ?")
        .bind(event)
        .execute(&pool)
        .await
        .unwrap();
    let body: Value = server
        .post("/v1/jobs/appointment-reminders")
        .add_header("Authorization", "job-secret")
        .await
        .json();
    assert_eq!(body["processed"], 0);
    assert!(sent.emails.lock().unwrap().is_empty());
    let cancelled: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM appointment_reminders WHERE event_id = ? AND status = 'cancelled'
            AND error_message = 'replaced_by_schedule_rule'",
    )
    .bind(event)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cancelled, 2);
}
