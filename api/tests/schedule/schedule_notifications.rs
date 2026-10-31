use crate::common;

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

fn email_template() -> Value {
    json!({
        "name": "Sink", "channel": "email",
        "subject": "Your installation on {{appointment.date}}",
        "body": "Hi {{customer.first_name}}, please disconnect the sink 2 hours before.",
    })
}

fn flow(template: i64, is_default: bool) -> Value {
    json!({
        "calendar_id": 8830, "name": "Installation", "is_default": is_default,
        "steps": [
            { "template_id": template, "offset_value": 1, "offset_unit": "hours", "send_time": null },
            { "template_id": template, "offset_value": 1, "offset_unit": "days", "send_time": "09:00" },
        ],
    })
}

#[test_db::test(seed)]
async fn admins_manage_templates_and_flows(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let (server, _) = server(&pool).await;
    let list = || async {
        server
            .get("/v1/schedule/notifications")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>()
    };
    server
        .get("/v1/schedule/notifications")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let created: Value = server
        .post("/v1/schedule/notifications/templates")
        .with_session(sessions::ADMIN)
        .json(&email_template())
        .await
        .json();
    let template = created["id"].as_i64().unwrap();
    let mut sms = email_template();
    sms["channel"] = json!("sms");
    let refused = server
        .post("/v1/schedule/notifications/templates")
        .with_session(sessions::ADMIN)
        .json(&sms)
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Texting is not set up for this company"
    );
    for (field, value) in [
        ("name", json!(" ")),
        ("subject", json!(" ")),
        ("body", json!("")),
    ] {
        let mut body = email_template();
        body[field] = value;
        server
            .post("/v1/schedule/notifications/templates")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    // Bad flows: another company's calendar, unknown template, bad timing.
    for (pointer, value) in [
        ("/calendar_id", json!(8832)),
        ("/steps/0/template_id", json!(999_999)),
        ("/steps/0/offset_value", json!(0)),
        ("/steps/1/offset_value", json!(31)),
        ("/steps/1/send_time", json!("9am")),
        ("/steps/0/send_time", json!("09:00")),
        ("/steps", json!([])),
    ] {
        let mut body = flow(template, true);
        *body.pointer_mut(pointer).unwrap() = value;
        server
            .post("/v1/schedule/notifications/flows")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    let first: Value = server
        .post("/v1/schedule/notifications/flows")
        .with_session(sessions::ADMIN)
        .json(&flow(template, true))
        .await
        .json();
    let first = first["id"].as_i64().unwrap();
    let body = list().await;
    assert_eq!(body["sms_enabled"], false);
    assert_eq!(body["templates"][0]["name"], "Sink");
    let saved = &body["flows"][0];
    assert_eq!(
        (saved["is_default"].as_bool(), saved["is_active"].as_bool()),
        (Some(true), Some(true))
    );
    // In the order they go out: the day before first.
    let steps = saved["steps"].as_array().unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|step| step["offset_unit"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["days", "hours"]
    );
    assert_eq!(steps[0]["send_time"], "09:00");
    let kept_step = steps[1]["id"].as_i64().unwrap();

    // A second default for the calendar takes over.
    let second: Value = server
        .post("/v1/schedule/notifications/flows")
        .with_session(sessions::ADMIN)
        .json(&flow(template, true))
        .await
        .json();
    let second = second["id"].as_i64().unwrap();
    let body = list().await;
    assert_eq!(body["flows"][0]["is_default"], false);
    assert_eq!(body["flows"][1]["is_default"], true);

    // Editing keeps the steps named by id and drops the rest.
    let mut edited = flow(template, false);
    edited["is_active"] = json!(false);
    edited["steps"] = json!([{ "id": kept_step, "template_id": template, "offset_value": 2, "offset_unit": "hours" }]);
    server
        .put(&format!("/v1/schedule/notifications/flows/{first}"))
        .with_session(sessions::ADMIN)
        .json(&edited)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = list().await;
    let steps = body["flows"][0]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        (steps[0]["id"].as_i64(), steps[0]["offset_value"].as_i64()),
        (Some(kept_step), Some(2))
    );
    assert_eq!(body["flows"][0]["is_active"], false);
    server
        .put(&format!("/v1/schedule/notifications/flows/{first}"))
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&edited)
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // A template in use cannot be deleted.
    let refused = server
        .delete(&format!("/v1/schedule/notifications/templates/{template}"))
        .with_session(sessions::ADMIN)
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "Remove it from these flows first: Installation"
    );
    for id in [first, second] {
        server
            .delete(&format!("/v1/schedule/notifications/flows/{id}"))
            .with_session(sessions::ADMIN)
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    server
        .delete(&format!("/v1/schedule/notifications/flows/{first}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .delete(&format!("/v1/schedule/notifications/templates/{template}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = list().await;
    assert_eq!(body["flows"].as_array().unwrap().len(), 0);
    assert_eq!(body["templates"].as_array().unwrap().len(), 0);

    // With texting set up a text template is allowed; without it, it is hidden.
    sqlx::raw_sql(TEXTING).execute(&pool).await.unwrap();
    server
        .post("/v1/schedule/notifications/templates")
        .with_session(sessions::ADMIN)
        .json(&sms)
        .await
        .assert_status(StatusCode::CREATED);
    let body = list().await;
    assert_eq!(body["sms_enabled"], true);
    assert_eq!(body["templates"][0]["subject"], Value::Null);
    sqlx::query("UPDATE company SET cloudtalk_access_key = NULL WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(list().await["templates"].as_array().unwrap().len(), 0);
}

#[test_db::test(seed)]
async fn confirmations_stay_out_of_flows(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let (server, _) = server(&pool).await;
    let create = |body: Value| {
        let server = &server;
        async move {
            let created: Value = server
                .post("/v1/schedule/notifications/templates")
                .with_session(sessions::ADMIN)
                .json(&body)
                .await
                .json();
            created["id"].as_i64().unwrap()
        }
    };
    let reminder = create(email_template()).await;
    let mut body = email_template();
    body["name"] = json!("Confirmation");
    body["kind"] = json!("confirmation");
    let confirmation = create(body.clone()).await;
    let list: Value = server
        .get("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(list["templates"][0]["kind"], "reminder");
    assert_eq!(list["templates"][1]["kind"], "confirmation");

    // A confirmation can't be a step.
    let refused = server
        .post("/v1/schedule/notifications/flows")
        .with_session(sessions::ADMIN)
        .json(&flow(confirmation, true))
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "A confirmation can't go in a flow; make the template a reminder first"
    );

    // A reminder a flow sends can't become a confirmation.
    server
        .post("/v1/schedule/notifications/flows")
        .with_session(sessions::ADMIN)
        .json(&flow(reminder, true))
        .await
        .assert_status(StatusCode::CREATED);
    let mut edited = email_template();
    edited["kind"] = json!("confirmation");
    let refused = server
        .put(&format!("/v1/schedule/notifications/templates/{reminder}"))
        .with_session(sessions::ADMIN)
        .json(&edited)
        .await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>()["error"],
        "A confirmation can't go in a flow. Remove it from these flows first: Installation"
    );

    // Left out, the kind is kept; a confirmation made a reminder can go in a flow.
    body.as_object_mut().unwrap().remove("kind");
    server
        .put(&format!(
            "/v1/schedule/notifications/templates/{confirmation}"
        ))
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let kind = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT CAST(kind AS CHAR) FROM schedule_notification_templates WHERE id = ?",
        )
        .bind(confirmation)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(kind().await, "confirmation");
    body["kind"] = json!("reminder");
    server
        .put(&format!(
            "/v1/schedule/notifications/templates/{confirmation}"
        ))
        .with_session(sessions::ADMIN)
        .json(&body)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(kind().await, "reminder");
    server
        .post("/v1/schedule/notifications/flows")
        .with_session(sessions::ADMIN)
        .json(&flow(confirmation, false))
        .await
        .assert_status(StatusCode::CREATED);
}

/// A flow on the calendar with one step 1 day before, sending a new
/// template; returns the flow id.
async fn rule(pool: &MySqlPool, calendar: i32, channel: &str, body: &str, is_default: bool) -> i32 {
    let template = sqlx::query(
        "INSERT INTO schedule_notification_templates (company_id, name, channel, subject, body)
         VALUES (100, 'Reminder', ?, IF(? = 'email', 'Reminder for {{customer.first_name}}', NULL), ?)",
    )
    .bind(channel)
    .bind(channel)
    .bind(body)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    let flow = sqlx::query(
        "INSERT INTO schedule_notification_flows (company_id, calendar_id, name, is_default)
         VALUES (100, ?, 'Reminders', ?)",
    )
    .bind(calendar)
    .bind(is_default)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_id();
    sqlx::query(
        "INSERT INTO schedule_notification_rules
           (company_id, flow_id, template_id, offset_value, offset_unit, created_at)
         VALUES (100, ?, ?, 1, 'days', '2000-01-01')",
    )
    .bind(flow)
    .bind(template)
    .execute(pool)
    .await
    .unwrap();
    i32::try_from(flow).unwrap()
}

/// An event `hours` from now, made long ago (or just now); a title that is a
/// customer id is that customer's event.
async fn event(pool: &MySqlPool, calendar: i32, title: &str, hours: i32, made_now: bool) {
    sqlx::query(
        "INSERT INTO events (title, start_date, end_date, created_user_id, calendar_id, location, created_date,
                             customer_id)
         VALUES (?, UTC_TIMESTAMP() + INTERVAL ? HOUR, UTC_TIMESTAMP() + INTERVAL ? HOUR, 100, ?, '1 Main St',
                 IF(?, CURRENT_TIMESTAMP, '2000-01-01'), ?)",
    )
    .bind(title)
    .bind(hours)
    .bind(hours + 2)
    .bind(calendar)
    .bind(made_now)
    .bind(title.parse::<i32>().ok())
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
        true,
    )
    .await;
    // Due: tomorrow minus a day is now.
    event(&pool, 8830, "1000", 23, false).await;
    // Not due yet.
    event(&pool, 8830, "1000", 30, false).await;
    // No customer, and booked after its send time: skipped.
    event(&pool, 8830, "Crew meeting", 23, false).await;
    event(&pool, 8830, "1000", 22, true).await;
    // Its time 4 hours ago (the appointment moved, or the step changed): too
    // late for "1 day before", skipped.
    event(&pool, 8830, "1000", 20, false).await;
    let (server, sent) = server(&pool).await;
    let run = || {
        server
            .post("/v1/jobs/appointment-reminders")
            .add_header("Authorization", "job-secret")
    };
    let body: Value = run().await.json();
    assert_eq!(
        (
            &body["rule_processed"],
            &body["rule_sent"],
            &body["rule_failed"]
        ),
        (&json!(1), &json!(1), &json!(0))
    );
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "Reminder for Alice");
    assert!(
        emails[0]
            .body
            .starts_with("<p>Hi Alice, your installation is ")
    );
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
            "skipped:late",
            "skipped:no_customer"
        ]
    );
    let again: Value = run().await.json();
    assert_eq!(again["rule_processed"], 0);
    assert_eq!(sent.emails.lock().unwrap().len(), 1);
}

#[test_db::test(seed)]
async fn after_rules_go_out_after_the_end(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let flow = rule(
        &pool,
        8830,
        "email",
        "Hi {{customer.first_name}}, we hope the installation went well.",
        true,
    )
    .await;
    sqlx::query("UPDATE schedule_notification_rules SET direction = 'after' WHERE flow_id = ?")
        .bind(flow)
        .execute(&pool)
        .await
        .unwrap();
    // Ended 25 hours ago: a day after is due.
    event(&pool, 8830, "1000", -27, false).await;
    // Ended 18 hours ago, and still to come: not due.
    event(&pool, 8830, "1000", -20, false).await;
    event(&pool, 8830, "1000", 5, false).await;
    let (server, sent) = server(&pool).await;
    let body: Value = server
        .post("/v1/jobs/appointment-reminders")
        .add_header("Authorization", "job-secret")
        .await
        .json();
    assert_eq!(
        (&body["rule_sent"], &body["rule_failed"]),
        (&json!(1), &json!(0))
    );
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(
        emails[0].body,
        "<p>Hi Alice, we hope the installation went well.</p>"
    );
}

#[test_db::test(seed)]
async fn text_rules_go_from_the_office_manager(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    rule(
        &pool,
        8830,
        "sms",
        "Hi {{customer.first_name}}, see you {{appointment.date}} at {{appointment.time}}.",
        true,
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
    rule(&pool, 8831, "email", "Hi {{customer.first_name}}", true).await;
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

async fn last_event(pool: &MySqlPool) -> i32 {
    sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn events_get_the_default_flow_or_the_ones_they_pick(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let default = rule(
        &pool,
        8830,
        "email",
        "Default {{customer.first_name}}",
        true,
    )
    .await;
    let other = rule(
        &pool,
        8830,
        "email",
        "Picked {{customer.first_name}}",
        false,
    )
    .await;
    // Gets the default only.
    event(&pool, 8830, "1000", 23, false).await;
    // Turned the default off and picked the other one instead.
    event(&pool, 8830, "1000", 23, false).await;
    let swapped = last_event(&pool).await;
    // Turned the default off, picked nothing.
    event(&pool, 8830, "1000", 23, false).await;
    let silent = last_event(&pool).await;
    for (table, event, flow) in [
        ("event_notification_skips", swapped, default),
        ("event_notification_picks", swapped, other),
        ("event_notification_skips", silent, default),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} (event_id, flow_id) VALUES (?, ?)"
        )))
        .bind(event)
        .bind(flow)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (server, sent) = server(&pool).await;
    let active: Value = server
        .get(&format!(
            "/v1/schedule/notifications/active?event_id={swapped}"
        ))
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(active["flows"].as_array().unwrap().len(), 2);
    assert_eq!(active["skipped_flow_ids"], json!([default]));
    assert_eq!(active["picked_flow_ids"], json!([other]));
    let body: Value = server
        .post("/v1/jobs/appointment-reminders")
        .add_header("Authorization", "job-secret")
        .await
        .json();
    assert_eq!(body["rule_sent"], 2);
    let mut bodies: Vec<String> = sent
        .emails
        .lock()
        .unwrap()
        .iter()
        .map(|email| email.body.clone())
        .collect();
    bodies.sort();
    assert_eq!(bodies, ["<p>Default Alice</p>", "<p>Picked Alice</p>"]);
}

#[test_db::test(seed)]
async fn a_confirmation_goes_out_right_away(pool: MySqlPool) {
    sqlx::raw_sql(CALENDARS).execute(&pool).await.unwrap();
    let template = sqlx::query(
        "INSERT INTO schedule_notification_templates (company_id, name, channel, subject, body)
         VALUES (100, 'Confirmation', 'email', 'Booked, {{customer.first_name}}',
                 'See you {{appointment.date}} at {{appointment.address}}.')",
    )
    .execute(&pool)
    .await
    .unwrap()
    .last_insert_id();
    // Two weeks out: far before any flow step would send.
    event(&pool, 8830, "1000", 14 * 24, true).await;
    let booked = last_event(&pool).await;
    event(&pool, 8830, "Crew meeting", 2, true).await;
    let crew = last_event(&pool).await;
    let (server, sent) = server(&pool).await;
    let confirm = |event: i32, template: u64| {
        server
            .post(&format!("/v1/events/{event}/confirmation"))
            .with_session(sessions::REP)
            .json(&json!({ "template_id": template }))
    };

    let body: Value = confirm(booked, template).await.json();
    assert_eq!(body, json!({ "channel": "email" }));
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].subject, "Booked, Alice");
    assert!(emails[0].body.starts_with("<p>See you "));
    assert!(emails[0].body.ends_with(" at 1 Main St.</p>"));
    let history: Vec<String> =
        sqlx::query_scalar("SELECT message FROM event_history WHERE event_id = ?")
            .bind(booked)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(history, ["Confirmation sent: Confirmation"]);

    confirm(crew, template).await.assert_status_bad_request();
    confirm(booked, template + 1)
        .await
        .assert_status_bad_request();
    assert_eq!(sent.emails.lock().unwrap().len(), 1);

    // The event's own email (a spouse's) gets it instead of the customer's.
    sqlx::query("UPDATE events SET contact_email = 'wife@example.com' WHERE id = ?")
        .bind(booked)
        .execute(&pool)
        .await
        .unwrap();
    confirm(booked, template).await.assert_status_ok();
    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 2);
    assert_eq!(emails[1].to, ["wife@example.com"]);
}
