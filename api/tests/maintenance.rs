mod common;

use axum::http::StatusCode;
use chrono::{Days, Utc};
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Rita (user 100) becomes company 100's shop manager.
async fn make_manager(pool: &MySqlPool) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (100, 3, 100)",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn in_days(days: u64) -> String {
    Utc::now()
        .date_naive()
        .checked_add_days(Days::new(days))
        .unwrap()
        .to_string()
}

async fn page(server: &axum_test::TestServer) -> Value {
    server
        .get("/v1/shop/maintenance")
        .with_session(sessions::REP)
        .await
        .json()
}

#[test_db::test(seed)]
async fn managers_set_up_equipment_and_checks(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .post("/v1/shop/maintenance/items")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Saw" }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(page(&server).await["can_edit"], false);
    make_manager(&pool).await;
    server
        .post("/v1/shop/maintenance/items")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Saw", "hours_per_day": 25 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/shop/maintenance/items")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Saw", "hours_per_day": 8, "days_per_month": 20 }))
        .await
        .assert_status(StatusCode::CREATED);
    let body = page(&server).await;
    assert_eq!(body["can_edit"], true);
    let item = body["items"][0]["id"].as_i64().unwrap();
    assert_eq!(body["items"][0]["usage_unit"], "hours");

    let check = json!({
        "maintenance_item_id": item, "name": "Blade",
        "conditions": [{ "type": "hours", "interval_value": 100 }, { "type": "days", "interval_value": 30 }],
    });
    let mut twice = check.clone();
    twice["conditions"] =
        json!([{ "type": "days", "interval_value": 1 }, { "type": "days", "interval_value": 2 }]);
    server
        .post("/v1/shop/maintenance/checks")
        .with_session(sessions::REP)
        .json(&twice)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    let res = server
        .post("/v1/shop/maintenance/checks")
        .with_session(sessions::REP)
        .json(&check)
        .await;
    res.assert_status(StatusCode::CREATED);
    let check_id = res.json::<Value>()["id"].as_i64().unwrap();
    let body = page(&server).await;
    let conditions = body["conditions"].as_array().unwrap();
    // 8 h × 20 days / 30 = 5.33 h a day: 100 h in 19 days.
    assert_eq!(conditions[0]["next_due_date"], in_days(19));
    assert_eq!(conditions[1]["next_due_date"], in_days(30));
    let hours_condition = conditions[0]["id"].as_i64().unwrap();

    server
        .put(&format!(
            "/v1/shop/maintenance/conditions/{hours_condition}/hours"
        ))
        .with_session(sessions::REP)
        .json(&json!({ "hours_done": 50 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = page(&server).await;
    assert_eq!(body["conditions"][0]["hours_done"], 50.0);
    assert_eq!(body["conditions"][0]["next_due_date"], in_days(10));

    server
        .post(&format!("/v1/shop/maintenance/checks/{check_id}/complete"))
        .with_session(sessions::REP)
        .json(&json!({ "notes": "New blade", "total_hours_of_work": 1234 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = page(&server).await;
    assert_eq!(body["conditions"][0]["hours_done"], 0.0);
    assert_eq!(body["conditions"][0]["next_due_date"], in_days(19));
    assert_eq!(
        body["completions"][0]["notes"],
        "Total hours of work: 1234\nNew blade"
    );
    assert_eq!(body["items"][0]["starting_hours"], 1234.0);
    assert!(body["checks"][0]["last_completed_at"].is_string());

    server
        .delete(&format!("/v1/shop/maintenance/items/{item}"))
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body = page(&server).await;
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["checks"], json!([]));
    assert_eq!(body["conditions"], json!([]));
}

#[test_db::test(seed)]
async fn hour_conditions_need_usage_numbers(pool: MySqlPool) {
    make_manager(&pool).await;
    let server = server(pool);
    server
        .post("/v1/shop/maintenance/items")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Forklift", "usage_unit": "miles" }))
        .await
        .assert_status(StatusCode::CREATED);
    let item = page(&server).await["items"][0]["id"].as_i64().unwrap();
    let res = server
        .post("/v1/shop/maintenance/checks")
        .with_session(sessions::REP)
        .json(&json!({
            "maintenance_item_id": item, "name": "Oil",
            "conditions": [{ "type": "hours", "interval_value": 500 }],
        }))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    // Its own numbers are enough.
    server
        .post("/v1/shop/maintenance/checks")
        .with_session(sessions::REP)
        .json(&json!({
            "maintenance_item_id": item, "name": "Oil", "hours_per_day": 30, "days_per_month": 30,
            "usage_unit": "miles", "conditions": [{ "type": "hours", "interval_value": 300 }],
        }))
        .await
        .assert_status(StatusCode::CREATED);
    let body = page(&server).await;
    assert_eq!(body["conditions"][0]["next_due_date"], in_days(10));
    let other: Value = server
        .get("/v1/shop/maintenance")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(other["items"], json!([]));
}
