use crate::common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Installations calendar 9800 with installers Mark (101) and Ada (102).
/// Alice's (1000) sale 9810: 50 sq ft, a kitchen (two slabs, "X 2"
/// waterfalls, 4" backsplash, one sink) and an island with a cooktop.
/// Unscheduled, oldest first: one not ready, Alice's install at Stop B
/// (ready), a 2-hour job at Stop A (ready), one with no length (ready) and a
/// 1-hour job at Stop A (ready). Mark is at Stop A on Monday 2026-11-02 from
/// 9:00 to 15:00 (EST, UTC-5). The shop is at Home.
async fn seed(pool: &MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug) VALUES
           (9800, 100, 'Installations', 'blue', 'installations'),
           (9801, 100, 'Crew', 'red', NULL);
         INSERT INTO users_positions (user_id, position_id, company_id) VALUES
           (101, 6, 100), (102, 6, 100);
         UPDATE company SET address = 'Home' WHERE id = 100;
         INSERT INTO sales (id, customer_id, seller_id, company_id, price, status, project_address, sale_date, square_feet) VALUES
           (9810, 1000, 100, 100, 5000, 'pending', '1 Main St', '2026-10-01 10:00:00', 50);
         INSERT INTO slab_inventory (id, stone_id, bundle, sale_id, room_uuid, room, waterfall, backsplash, tear_out, stove) VALUES
           (98101, 3000, 'JT-1', 9810, UUID_TO_BIN('11111111-1111-4111-8111-111111111111'), 'kitchen', 'X 2', '4 inch', 'no', 'f/s'),
           (98102, 3000, 'JT-2', 9810, UUID_TO_BIN('11111111-1111-4111-8111-111111111111'), 'kitchen', 'X 2', '4 inch', 'no', 'f/s'),
           (98103, 3000, 'JT-3', 9810, UUID_TO_BIN('22222222-2222-4222-8222-222222222222'), 'island', 'no', 'no', 'no', 'c/t');
         INSERT INTO sinks (id, sale_id, slab_id, is_deleted) VALUES (98110, 9810, 98101, 0), (98111, 9810, 98102, 1);
         INSERT INTO events (id, title, location, start_date, end_date, created_user_id, calendar_id, customer_id, unscheduled, status, created_date, assigned_user_id) VALUES
           (9820, 'Alice install', 'Stop B', '2026-10-20 12:00:00', '2026-10-20 12:00:00', 100, 9800, 1000, 1, 'ready', '2026-10-20 10:00:00', NULL),
           (9821, 'Small job', 'Stop A', '2026-10-20 12:00:00', '2026-10-20 14:00:00', 100, 9800, NULL, 1, 'ready', '2026-10-21 10:00:00', NULL),
           (9822, 'Not ready', NULL, '2026-10-20 12:00:00', '2026-10-20 14:00:00', 100, 9800, NULL, 1, 'not_ready', '2026-10-19 10:00:00', NULL),
           (9823, 'No length', NULL, '2026-10-20 12:00:00', '2026-10-20 12:00:00', 100, 9800, NULL, 1, 'ready', '2026-10-22 10:00:00', NULL),
           (9824, 'Mark busy', 'Stop A', '2026-11-02 14:00:00', '2026-11-02 20:00:00', 100, 9801, NULL, 0, 'scheduled', '2026-10-22 10:00:00', 101),
           (9825, 'Near job', 'Stop A', '2026-10-20 12:00:00', '2026-10-20 13:00:00', 100, 9800, NULL, 1, 'ready', '2026-10-23 10:00:00', NULL);",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn install_rules() -> Value {
    json!({
        "base_minutes": 30,
        "per_sqft_minutes": 3.0,
        "per_waterfall_minutes": 30,
        "per_sink_minutes": 20,
        "per_cooktop_minutes": 15,
        "per_backsplash_minutes": 10,
        "per_full_backsplash_minutes": 0,
        "per_tear_out_minutes": 0,
        "travel_minutes": 30,
        "day_start_minute": 540,
        "day_end_minute": 1020,
        "work_days": [1, 2, 3, 4, 5],
    })
}

#[test_db::test(seed)]
async fn admins_set_the_job_time_rules(pool: MySqlPool) {
    let server = server(pool);
    let rules = server
        .get("/v1/schedule/time-rules")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(rules["installations"]["base_minutes"], 0);
    assert_eq!(rules["installations"]["travel_minutes"], 30);
    assert_eq!(rules["templates"]["work_days"], json!([1, 2, 3, 4, 5]));

    let body = json!({ "kind": "installations", "rules": install_rules() });
    server
        .put("/v1/schedule/time-rules")
        .json(&body)
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/schedule/time-rules")
        .json(&body)
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let rules = server
        .get("/v1/schedule/time-rules")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(rules["installations"], install_rules());
    assert_eq!(rules["templates"]["base_minutes"], 0);

    let mut bad = install_rules();
    bad["day_end_minute"] = json!(500);
    server
        .put("/v1/schedule/time-rules")
        .json(&json!({ "kind": "templates", "rules": bad }))
        .with_session(sessions::ADMIN)
        .await
        .assert_status_bad_request();
    let mut bad = install_rules();
    bad["work_days"] = json!([]);
    server
        .put("/v1/schedule/time-rules")
        .json(&json!({ "kind": "templates", "rules": bad }))
        .with_session(sessions::ADMIN)
        .await
        .assert_status_bad_request();
}

#[test_db::test(seed)]
async fn a_job_is_sized_from_the_customers_sale(pool: MySqlPool) {
    seed(&pool).await;
    let server = server(pool);
    let time = |query: &'static str| {
        let server = &server;
        async move {
            server
                .get(&format!("/v1/schedule/job-time?{query}"))
                .with_session(sessions::REP)
                .await
                .json::<Value>()
        }
    };
    // No rules yet: nothing to size with.
    assert_eq!(time("calendar_id=9800&customer_id=1000").await, Value::Null);
    server
        .put("/v1/schedule/time-rules")
        .json(&json!({ "kind": "installations", "rules": install_rules() }))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let job = time("calendar_id=9800&customer_id=1000").await;
    // 30 + 50 × 3 + 2 × 30 + 20 + 15 + 10 = 285.
    assert_eq!(job["minutes"], 285);
    assert_eq!(job["sale_id"], 9810);
    assert_eq!(
        job["summary"],
        "50 sq ft · 2 waterfalls · 1 sink · 1 cooktop · 4\" backsplash (1 room)"
    );
    assert_eq!(job["lines"][0], json!({ "label": "Setup", "minutes": 30 }));
    assert_eq!(
        job["lines"][1],
        json!({ "label": "50 sq ft", "minutes": 150 })
    );
    assert_eq!(time("calendar_id=9800&sale_id=9810").await["minutes"], 285);
    // Not an installations or templates calendar, or no sale.
    assert_eq!(time("calendar_id=9801&customer_id=1000").await, Value::Null);
    assert_eq!(time("calendar_id=9800&customer_id=1001").await, Value::Null);
}

/// Drive minutes: Home to Stop A 20, to Stop B 60, A to B 50; else 30.
fn drive(from: &str, to: &str) -> u32 {
    match (from, to) {
        (a, b) if a == b => 0,
        ("Home", "Stop A") | ("Stop A", "Home") => 20,
        ("Home", "Stop B") | ("Stop B", "Home") => 60,
        ("Stop A", "Stop B") | ("Stop B", "Stop A") => 50,
        _ => 30,
    }
}

/// A server whose Google route matrix answers with [`drive`].
async fn server_with_drives(pool: MySqlPool) -> TestServer {
    let router = Router::new().route(
        "/matrix",
        post(|Json(body): Json<Value>| async move {
            let name = |point: &Value| point["waypoint"]["address"].as_str().unwrap().to_string();
            let origins: Vec<String> = body["origins"]
                .as_array()
                .unwrap()
                .iter()
                .map(name)
                .collect();
            let destinations: Vec<String> = body["destinations"]
                .as_array()
                .unwrap()
                .iter()
                .map(name)
                .collect();
            let mut elements = Vec::new();
            for (o, from) in origins.iter().enumerate() {
                for (d, to) in destinations.iter().enumerate() {
                    elements.push(json!({
                        "originIndex": o, "destinationIndex": d, "condition": "ROUTE_EXISTS",
                        "duration": format!("{}s", drive(from, to) * 60),
                    }));
                }
            }
            Json(Value::Array(elements))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_maps(Maps::new(
        Some("key"),
        &format!("{base}/matrix"),
        &base,
    ));
    TestServer::new(app(state))
}

#[test_db::test(seed)]
async fn auto_plan_fills_one_installers_days_by_drive_time(pool: MySqlPool) {
    seed(&pool).await;
    let server = server_with_drives(pool.clone()).await;
    server
        .put("/v1/schedule/time-rules")
        .json(&json!({ "kind": "installations", "rules": install_rules() }))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let request = |user: Value| {
        json!({
            "calendar_id": 9800, "user_id": user, "start_date": "2026-11-02",
            "days": 5, "statuses": ["ready"],
        })
    };
    let plan = server
        .post("/v1/schedule/auto-plan")
        .json(&request(json!(102)))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    // Ada's Monday opens with the oldest job, Alice's at Stop B: 60 minutes
    // from the shop, 10:00 to 14:45. After the 50-minute drive to Stop A the
    // 2-hour job no longer fits, the 1-hour one does (15:45). Tuesday opens
    // with the 2-hour job, 20 minutes from the shop: 9:30. Mark's day is not
    // Ada's.
    assert_eq!(
        plan["placements"],
        json!([
            {
                "event_id": 9820, "title": "Alice install", "customer_name": "Alice Lead",
                "location": "Stop B", "assigned_user_id": 102,
                "start_date": "2026-11-02T15:00:00Z", "end_date": "2026-11-02T19:45:00Z",
                "minutes": 285, "drive_minutes": 60,
                "job": "50 sq ft · 2 waterfalls · 1 sink · 1 cooktop · 4\" backsplash (1 room)",
                "longer_than_day": false,
            },
            {
                "event_id": 9825, "title": "Near job", "customer_name": null,
                "location": "Stop A", "assigned_user_id": 102,
                "start_date": "2026-11-02T20:45:00Z", "end_date": "2026-11-02T21:45:00Z",
                "minutes": 60, "drive_minutes": 50, "job": null, "longer_than_day": false,
            },
            {
                "event_id": 9821, "title": "Small job", "customer_name": null,
                "location": "Stop A", "assigned_user_id": 102,
                "start_date": "2026-11-03T14:30:00Z", "end_date": "2026-11-03T16:30:00Z",
                "minutes": 120, "drive_minutes": 20, "job": null, "longer_than_day": false,
            },
        ])
    );
    assert_eq!(
        plan["skipped"],
        json!([{
            "event_id": 9823, "title": "No length",
            "reason": "No sale to size it from, and the event has no length.",
        }])
    );
    assert_eq!(plan["drive_note"], Value::Null);
    // Nothing was saved.
    let waiting: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE id IN (9820, 9821, 9823, 9825) AND unscheduled = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(waiting, 4);

    // Mark is at Stop A until 15:00 on Monday: the 2-hour job there fills
    // the rest of his day (no drive), and the others go to Tuesday.
    let mark = server
        .post("/v1/schedule/auto-plan")
        .json(&request(json!(101)))
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let ids: Vec<i64> = mark["placements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|placement| placement["event_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [9821, 9820, 9825]);
    assert_eq!(mark["placements"][0]["start_date"], "2026-11-02T20:00:00Z");
    assert_eq!(mark["placements"][0]["drive_minutes"], 0);
    assert_eq!(mark["placements"][1]["start_date"], "2026-11-03T15:00:00Z");

    for bad in [
        // No installer picked, someone not an installer, not a job calendar,
        // no days.
        request(Value::Null),
        request(json!(104)),
        json!({ "calendar_id": 9801, "user_id": 102, "start_date": "2026-11-02", "days": 5, "statuses": ["ready"] }),
        json!({ "calendar_id": 9800, "user_id": 102, "start_date": "2026-11-02", "days": 0, "statuses": ["ready"] }),
    ] {
        server
            .post("/v1/schedule/auto-plan")
            .json(&bad)
            .with_session(sessions::REP)
            .await
            .assert_status_bad_request();
    }
}
