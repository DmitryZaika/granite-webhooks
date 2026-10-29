use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

/// Vic (104, no employee or admin flag) holds `position` in Acme (100); Ada
/// (102, admin) is Acme's Manager, who may change position access.
async fn seat(pool: &MySqlPool, position: i32) {
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (104, ?, 100), (102, 5, 100)",
    )
    .bind(position)
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn a_scheduling_manager_reaches_only_customers_and_the_schedule(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::VIEWER)
        .await
        .json();
    assert_eq!(shell["sections"], json!(["customers", "schedule"]));
    for path in ["/v1/customers", "/v1/calendars", "/v1/me"] {
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status_ok();
    }
    for path in ["/v1/stones", "/v1/transactions", "/v1/admin/users"] {
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    // Everyone else keeps the whole app.
    let rep: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(rep["sections"], Value::Null);
    server
        .get("/v1/stones")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_full_position_beside_it_keeps_the_whole_app(pool: MySqlPool) {
    seat(&pool, 12).await;
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (104, 1, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::VIEWER)
        .await
        .json();
    assert_eq!(shell["sections"], Value::Null);
}

#[test_db::test(seed)]
async fn the_company_changes_and_resets_a_positions_sections(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    let access: Value = server
        .get("/v1/company/position-sections")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let scheduling = &access["positions"][1];
    assert_eq!(scheduling["position_id"], 13);
    assert_eq!(scheduling["customized"], false);
    assert_eq!(scheduling["sections"], json!(["customers", "schedule"]));

    server
        .put("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["schedule", "inventory"] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/customers")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/stones")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_ok();
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(sessions::VIEWER)
        .await
        .json();
    assert_eq!(shell["sections"], json!(["inventory", "schedule"]));

    server
        .delete("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/customers")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn only_managers_change_position_access_and_only_known_sections(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    for session in [sessions::REP, sessions::VIEWER] {
        server
            .put("/v1/company/position-sections/13")
            .with_session(session)
            .json(&json!({ "sections": ["inventory"] }))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .put("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["nope"] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/company/position-sections/1")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": [] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}
