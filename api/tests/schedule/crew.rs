use crate::common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use api::webhooks_client::{OutgoingEmail, WebhooksClient};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use chrono::{Duration, NaiveDate, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// About 69 miles per degree of latitude.
const MILES_PER_DEGREE: f64 = 69.0;
const SHOP_LAT: f64 = 39.9;
const LNG: f64 = -86.1;

/// The shop, and two jobs 5 and 10 miles north of it.
fn lat_of(address: &str) -> Option<f64> {
    let miles = match address {
        "Shop St" => 0.0,
        "1 Job Rd" => 5.0,
        "2 Job Rd" => 10.0,
        _ => return None,
    };
    Some(SHOP_LAT + miles / MILES_PER_DEGREE)
}

/// Emails sent, and a Google that geocodes `lat_of`'s addresses.
async fn server(pool: &MySqlPool) -> (TestServer, Arc<Mutex<Vec<OutgoingEmail>>>) {
    let emails: Arc<Mutex<Vec<OutgoingEmail>>> = Arc::default();
    let router = Router::new()
        .route(
            "/email/send",
            post(
                |State(emails): State<Arc<Mutex<Vec<OutgoingEmail>>>>,
                 Json(email): Json<OutgoingEmail>| async move {
                    emails.lock().unwrap().push(email);
                    Json(json!({ "message_id": "m-1" }))
                },
            ),
        )
        .route(
            "/geocode/json",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                Json(
                    match lat_of(query.get("address").map_or("", String::as_str)) {
                        Some(lat) => json!({
                            "status": "OK",
                            "results": [{ "geometry": { "location": { "lat": lat, "lng": LNG } } }],
                        }),
                        None => json!({ "status": "ZERO_RESULTS", "results": [] }),
                    },
                )
            }),
        )
        .with_state(emails.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(WebhooksClient::new(Some(&base), Some("key")))
        .with_maps(Maps::new(Some("key"), &format!("{base}/matrix"), &base));
    (TestServer::new(app(state)), emails)
}

fn store_today() -> NaiveDate {
    Utc::now().with_timezone(&Indianapolis).date_naive()
}

/// Mark (101) installs: 9900 today for Alice at 1 Job Rd, 9901 today at
/// 2 Job Rd, 9902 tomorrow. All day, so they stay on today's date.
async fn jobs(pool: &MySqlPool) {
    let today = store_today();
    let tomorrow = today + Duration::days(1);
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9890, 100, 'Installations', 'red', 'installations');
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES (101, 6, 100);
         UPDATE company SET address = 'Shop St' WHERE id = 100;",
    )
    .execute(pool)
    .await
    .unwrap();
    for (id, day, location, customer) in [
        (9900, today, "1 Job Rd", Some(1000)),
        (9901, today, "2 Job Rd", None),
        (9902, tomorrow, "1 Job Rd", None),
    ] {
        sqlx::query(
            "INSERT INTO events (id, title, location, start_date, end_date, all_day,
                                 created_user_id, assigned_user_id, calendar_id, customer_id)
             VALUES (?, 'Install', ?, ?, ?, 1, 100, 101, 9890, ?)",
        )
        .bind(id)
        .bind(location)
        .bind(day.and_hms_opt(0, 0, 0).unwrap())
        .bind(day.and_hms_opt(23, 59, 59).unwrap())
        .bind(customer)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn history(pool: &MySqlPool, event: i32) -> Vec<String> {
    sqlx::query_scalar("SELECT message FROM event_history WHERE event_id = ? ORDER BY id")
        .bind(event)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn crew_status(pool: &MySqlPool, event: i32) -> Option<String> {
    sqlx::query_scalar("SELECT crew_status FROM events WHERE id = ?")
        .bind(event)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn on_my_way_tells_the_customer_once_and_completed_finishes(pool: MySqlPool) {
    jobs(&pool).await;
    let (server, emails) = server(&pool).await;
    let tap = |id: i32, status: &str, session: &'static str| {
        server
            .post(&format!("/v1/events/{id}/crew-status"))
            .with_session(session)
            .json(&json!({ "status": status }))
    };

    // No message set up yet: the status is saved and says why nobody was told.
    let body: Value = tap(9900, "on_my_way", sessions::MANAGER).await.json();
    assert_eq!(body["status"], "on_my_way");
    assert_eq!(body["customer_told"], Value::Null);
    assert!(
        body["message_error"]
            .as_str()
            .unwrap()
            .contains("Admin > Schedule")
    );

    let message = "Hi {{customer.first_name}}, we are on the way to {{appointment.address}}.";
    server
        .put("/v1/schedule/notifications/on-my-way")
        .with_session(sessions::ADMIN)
        .json(&json!({ "message": message, "subject": " " }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let settings: Value = server
        .get("/v1/schedule/notifications")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(
        settings["on_my_way"],
        json!({ "message": message, "subject": null })
    );

    // Without texting it goes by email, once.
    let body: Value = tap(9900, "on_my_way", sessions::MANAGER).await.json();
    assert_eq!(body["customer_told"], "email");
    assert_eq!(body["message_error"], Value::Null);
    let again: Value = tap(9900, "on_my_way", sessions::MANAGER).await.json();
    assert_eq!(again["customer_told"], Value::Null);
    assert_eq!(again["message_error"], "The customer was already told");
    {
        let sent = emails.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].subject, "On the way");
        assert_eq!(
            sent[0].body,
            "<p>Hi Alice, we are on the way to 1 Job Rd.</p>"
        );
    }

    // Only the job's crew or an admin taps; tomorrow's job is not on the way yet.
    tap(9900, "on_my_way", sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    tap(9902, "on_my_way", sessions::MANAGER)
        .await
        .assert_status_bad_request();
    // Arrived comes only from the phone.
    tap(9900, "arrived", sessions::MANAGER)
        .await
        .assert_status_bad_request();

    let done: Value = tap(9900, "completed", sessions::MANAGER).await.json();
    assert_eq!(done["status"], "completed");
    tap(9900, "on_my_way", sessions::MANAGER)
        .await
        .assert_status_bad_request();
    assert_eq!(
        history(&pool, 9900).await,
        [
            "On my way (no message: No On my way message is set up (Admin > Schedule > Customer notifications))",
            "On my way: the customer got an email",
            "Completed",
        ]
    );

    // The crew's schedule shows it.
    let today = store_today();
    let range = format!(
        "/v1/me/schedule?start={}T00:00:00Z&end={}T00:00:00Z",
        today - Duration::days(1),
        today + Duration::days(2)
    );
    let mine: Value = server
        .get(&range)
        .with_session(sessions::MANAGER)
        .await
        .json();
    let job = mine["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["id"] == 9900)
        .unwrap();
    assert_eq!(job["crew_status"], "completed");
    assert_eq!(job["customer_told"], true);
    assert!(job["completed_at"].is_string());
    assert!(job["on_my_way_at"].is_string());
}

#[test_db::test(seed)]
async fn the_phone_marks_arrived_and_on_my_way_but_never_completed(pool: MySqlPool) {
    jobs(&pool).await;
    let (server, emails) = server(&pool).await;
    let at = |miles: f64, accuracy: f64| {
        server
            .post("/v1/me/location")
            .with_session(sessions::MANAGER)
            .json(&json!({
                "latitude": SHOP_LAT + miles / MILES_PER_DEGREE,
                "longitude": LNG,
                "accuracy_m": accuracy,
            }))
    };
    let changed = |arrived: Option<i32>, on_my_way: Option<i32>| json!({ "arrived": arrived, "on_my_way": on_my_way });

    // At the shop, then 0.3 mi out: nothing yet.
    assert_eq!(at(0.0, 10.0).await.json::<Value>(), changed(None, None));
    assert_eq!(at(0.3, 10.0).await.json::<Value>(), changed(None, None));
    // A far-off reading that is not accurate is ignored.
    assert_eq!(at(3.0, 900.0).await.json::<Value>(), changed(None, None));
    // Half a mile from the shop: on the way to the first job.
    assert_eq!(
        at(0.6, 10.0).await.json::<Value>(),
        changed(None, Some(9900))
    );
    // At its address.
    assert_eq!(
        at(5.02, 10.0).await.json::<Value>(),
        changed(Some(9900), None)
    );
    // Driving off: on the way to the next job; the first stays arrived.
    assert_eq!(
        at(5.6, 10.0).await.json::<Value>(),
        changed(None, Some(9901))
    );
    assert_eq!(crew_status(&pool, 9900).await.as_deref(), Some("arrived"));
    assert_eq!(crew_status(&pool, 9901).await.as_deref(), Some("on_my_way"));
    // Tomorrow's job is untouched, and no customer was told.
    assert_eq!(crew_status(&pool, 9902).await, None);
    assert!(emails.lock().unwrap().is_empty());
    assert_eq!(
        history(&pool, 9900).await,
        ["On my way (phone location)", "Arrived (phone location)"]
    );
    let left: Option<String> =
        sqlx::query_scalar("SELECT CAST(left_at AS CHAR) FROM events WHERE id = 9900")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(left.is_some());
}

#[test_db::test(seed)]
async fn moving_a_job_to_another_day_clears_the_crew_status(pool: MySqlPool) {
    jobs(&pool).await;
    let (server, _) = server(&pool).await;
    server
        .post("/v1/events/9901/crew-status")
        .with_session(sessions::MANAGER)
        .json(&json!({ "status": "completed" }))
        .await
        .assert_status_ok();
    let edit = |day: NaiveDate, title: &str| {
        json!({
            "title": title, "calendar_id": 9890, "all_day": true, "location": "2 Job Rd",
            "assigned_user_id": 101,
            "start_date": format!("{day} 00:00:00"), "end_date": format!("{day} 23:59:59"),
        })
    };
    let today = store_today();
    // Same day: kept.
    server
        .put("/v1/events/9901")
        .with_session(sessions::REP)
        .json(&edit(today, "Install top"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(crew_status(&pool, 9901).await.as_deref(), Some("completed"));
    // Another day: cleared.
    server
        .put("/v1/events/9901")
        .with_session(sessions::REP)
        .json(&edit(today + Duration::days(3), "Install top"))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(crew_status(&pool, 9901).await, None);
}
