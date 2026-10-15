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

// Alice Lead (1000, Rita's customer) books an estimate on calendar 8820.

#[derive(Clone, Default)]
struct Sent {
    texts: Arc<Mutex<Vec<OutgoingSms>>>,
    emails: Arc<Mutex<Vec<OutgoingEmail>>>,
}

async fn server(pool: &MySqlPool) -> (TestServer, Sent) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (8820, 100, 'Estimates', 'blue', 'estimates'), (8821, 100, 'Crew', 'teal', NULL);",
    )
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
                    Json(json!({ "ids": [1] }))
                },
            ),
        )
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
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN).with_webhooks(
        WebhooksClient::new(Some(&format!("http://{address}")), Some("key")),
    );
    (TestServer::new(app(state)), sent)
}

fn booking(customer: i64, calendar: i64) -> Value {
    json!({
        "customer_id": customer, "calendar_id": calendar, "title": "1000",
        "start_date": "2099-01-15 15:00:00", "end_date": "2099-01-15 16:00:00",
        "location": "1 Main St",
    })
}

#[test_db::test(seed)]
async fn an_estimate_is_emailed_without_a_texting_line(pool: MySqlPool) {
    let (server, sent) = server(&pool).await;
    let res = server
        .post("/v1/events/estimates")
        .with_session(sessions::REP)
        .json(&booking(1000, 8820))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body: Value = res.json();
    assert_eq!(body["notification_channel"], "email");
    let id = body["id"].as_i64().unwrap();

    let emails = sent.emails.lock().unwrap().clone();
    assert_eq!(emails.len(), 1);
    assert_eq!(emails[0].to, ["alice@example.com"]);
    assert_eq!(emails[0].subject, "Acme Stone - In-Home Estimate Scheduled");
    assert!(emails[0].transactional);
    assert!(
        emails[0]
            .body
            .starts_with("<p>Hi Alice, your in-home estimate with Rita is scheduled for ")
    );
    assert!(
        emails[0]
            .body
            .contains("January 15 at 10:00 AM – 11:00 AM.")
    );
    assert!(sent.texts.lock().unwrap().is_empty());

    let reminders: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM appointment_reminders WHERE event_id = ? AND status = 'pending')
              + (SELECT COUNT(*) FROM appointment_rep_reminders WHERE event_id = ? AND status = 'pending' AND user_id = 100)",
    )
    .bind(id)
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reminders, 4);
}

#[test_db::test(seed)]
async fn the_office_manager_texts_the_confirmation(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET cloudtalk_access_key = 'k', cloudtalk_access_secret = 's' WHERE id = 100;
         UPDATE users SET cloudtalk_agent_id = '5101', cloudtalk_phone_number = '3175550999' WHERE id = 101;
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 10, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, sent) = server(&pool).await;
    let res = server
        .post("/v1/events/estimates")
        .with_session(sessions::REP)
        .json(&booking(1000, 8820))
        .await;
    res.assert_status(StatusCode::CREATED);
    assert_eq!(res.json::<Value>()["notification_channel"], "sms");
    let texts = sent.texts.lock().unwrap().clone();
    assert_eq!(texts.len(), 1);
    assert_eq!(
        (
            texts[0].provider.as_str(),
            texts[0].user_id,
            texts[0].agent_id.as_str(),
            texts[0].phone_digits.as_str(),
            texts[0].is_automated
        ),
        ("cloudtalk", 101, "5101", "3175550100", true)
    );
    assert!(texts[0].text.ends_with("Address: 1 Main St."));
}

#[test_db::test(seed)]
async fn a_booking_needs_the_estimates_calendar_and_a_contact(pool: MySqlPool) {
    sqlx::raw_sql(
        "UPDATE customers SET phone = NULL, phone_2 = NULL WHERE id = 1001;
         DELETE FROM customers_emails WHERE customer_id = 1001;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (server, _) = server(&pool).await;
    let book = |body: Value| {
        let request = server
            .post("/v1/events/estimates")
            .with_session(sessions::REP)
            .json(&body);
        async move { request.await }
    };
    let crew = book(booking(1000, 8821)).await;
    crew.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        crew.json::<Value>()["error"],
        "Estimates calendar not found"
    );
    let silent = book(booking(1001, 8820)).await;
    silent.assert_status(StatusCode::BAD_REQUEST);
    book(booking(2000, 8820))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let sample = book(booking(990_000_021, 8820)).await;
    sample.assert_status(StatusCode::CREATED);
    assert_eq!(sample.json::<Value>()["id"], 0);
}
