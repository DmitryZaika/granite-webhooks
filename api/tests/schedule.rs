mod common;

use api::app;
use api::maps::Maps;
use api::state::AppState;
use axum::http::{StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Drive minutes between the company (H) and the stops: H→C is shortest,
/// then C→A, then A→B.
fn minutes(from: &str, to: &str) -> Option<u32> {
    let rank = |place: &str| {
        ["Home", "Stop A", "Stop B", "Stop C", "Nowhere"]
            .iter()
            .position(|p| *p == place)
    };
    match (rank(from)?, rank(to)?) {
        (_, 4) | (4, _) => None,
        (0, 3) | (3, 1) | (1, 2) => Some(5),
        _ => Some(30),
    }
}

async fn google() -> String {
    let router = Router::new()
        .route(
            "/matrix",
            post(|Json(body): Json<Value>| async move {
                let name = |point: &Value| point["waypoint"]["address"].as_str().unwrap().to_string();
                let origins: Vec<String> = body["origins"].as_array().unwrap().iter().map(name).collect();
                let destinations: Vec<String> = body["destinations"].as_array().unwrap().iter().map(name).collect();
                let mut elements = Vec::new();
                for (o, from) in origins.iter().enumerate() {
                    for (d, to) in destinations.iter().enumerate() {
                        elements.push(match minutes(from, to) {
                            Some(m) => json!({ "originIndex": o, "destinationIndex": d, "condition": "ROUTE_EXISTS", "duration": format!("{}s", m * 60) }),
                            None => json!({ "originIndex": o, "destinationIndex": d, "condition": "ROUTE_NOT_FOUND" }),
                        });
                    }
                }
                Json(Value::Array(elements))
            }),
        )
        .route(
            "/geocode/json",
            get(|| async { Json(json!({ "status": "OK", "results": [{ "geometry": { "location": { "lat": 39.9, "lng": -86.1 } } }] })) }),
        )
        .route(
            "/staticmap",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![137u8, 80, 78, 71]) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}")
}

async fn server(pool: MySqlPool) -> TestServer {
    let base = google().await;
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_maps(Maps::new(
        Some("key"),
        &format!("{base}/matrix"),
        &base,
    ));
    TestServer::new(app(state))
}

const DAY: &str = "
UPDATE company SET address = 'Home' WHERE id = 100;
INSERT INTO events (id, title, location, start_date, end_date, all_day, created_user_id) VALUES
  (9100, 'A', 'Stop A', '2030-01-15 14:00:00', '2030-01-15 15:00:00', 0, 100),
  (9101, 'B', 'Stop B', '2030-01-15 15:00:00', '2030-01-15 15:30:00', 0, 100),
  (9102, 'C', 'Stop C', '2030-01-15 16:00:00', '2030-01-15 18:00:00', 0, 100),
  (9103, 'Lost', 'Nowhere', '2030-01-15 17:00:00', '2030-01-15 18:00:00', 0, 100);
";

#[test_db::test(seed)]
async fn a_route_reorders_the_day(pool: MySqlPool) {
    sqlx::raw_sql(DAY).execute(&pool).await.unwrap();
    let server = server(pool.clone()).await;
    let res = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9101, 9102] }))
        .await;
    res.assert_status_ok();
    assert_eq!(
        res.json::<Value>(),
        json!({ "order": [9102, 9100, 9101], "changed": 3, "stops": 3 })
    );
    let times: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT id, CAST(start_date AS CHAR), CAST(end_date AS CHAR) FROM events WHERE id IN (9100, 9101, 9102) ORDER BY start_date",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        times,
        [
            (
                9102,
                "2030-01-15 14:00:00".into(),
                "2030-01-15 16:00:00".into()
            ),
            (
                9100,
                "2030-01-15 15:00:00".into(),
                "2030-01-15 16:00:00".into()
            ),
            (
                9101,
                "2030-01-15 16:00:00".into(),
                "2030-01-15 16:30:00".into()
            ),
        ]
    );

    let unreachable = server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100, 9103] }))
        .await;
    unreachable.assert_status(StatusCode::BAD_GATEWAY);
    assert_eq!(
        unreachable.json::<Value>()["error"],
        "Google Maps could not find a driving route to: Nowhere"
    );
    server
        .post("/v1/schedule/optimize-route")
        .with_session(sessions::REP)
        .json(&json!({ "event_ids": [9100] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[test_db::test(seed)]
async fn a_day_map_is_drawn(pool: MySqlPool) {
    sqlx::raw_sql(DAY).execute(&pool).await.unwrap();
    let body: Value = server(pool)
        .await
        .get("/v1/schedule/day-map?address=Stop%20A&address=Stop%20B")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(body["home_address"], "Home");
    assert_eq!(body["image"], "data:image/png;base64,iVBORw==");
}
