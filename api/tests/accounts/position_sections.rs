use crate::common;

use axum::http::StatusCode;
use axum_test::TestServer;
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

/// The tabs the app shell gives `session` (null: nothing limited).
async fn tabs(server: &TestServer, session: &str) -> Value {
    let shell: Value = server
        .get("/v1/app/shell")
        .with_session(session)
        .await
        .json();
    shell["sections"].clone()
}

#[test_db::test(seed)]
async fn a_scheduling_manager_sees_only_customers_and_the_schedule(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    assert_eq!(
        tabs(&server, sessions::VIEWER).await,
        json!(["customers", "schedule"])
    );
    // Data the open pages share stays readable.
    for path in ["/v1/customers", "/v1/calendars", "/v1/me", "/v1/stones"] {
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status_ok();
    }
    // Data only closed tabs use is refused.
    for path in ["/v1/checklists", "/v1/survey-results", "/v1/admin/users"] {
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    // A Sales Rep the company left alone keeps the whole app.
    assert_eq!(tabs(&server, sessions::REP).await, Value::Null);
    server
        .get("/v1/checklists")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn several_positions_open_all_their_tabs_together(pool: MySqlPool) {
    seat(&pool, 12).await;
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (104, 1, 100)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool);
    // Sales Rep's whole main app plus Inventory Manager's admin inventory.
    let open = tabs(&server, sessions::VIEWER).await;
    let open = open.as_array().unwrap();
    for key in ["dashboard", "transactions", "checklists", "admin_stones"] {
        assert!(open.contains(&json!(key)), "{key}: {open:?}");
    }
    assert!(!open.contains(&json!("admin_quotes")));
}

#[test_db::test(seed)]
async fn the_company_changes_and_resets_a_positions_tabs(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    let access: Value = server
        .get("/v1/company/position-sections")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let positions = access["positions"].as_array().unwrap();
    let ids: Vec<i64> = positions
        .iter()
        .map(|p| p["position_id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2, 5, 10, 3, 12, 13]);
    let scheduling = &positions[6];
    assert_eq!(scheduling["customized"], false);
    assert_eq!(scheduling["sections"], json!(["customers", "schedule"]));
    assert_eq!(positions[0]["customized"], false);
    assert!(
        positions[0]["sections"]
            .as_array()
            .unwrap()
            .contains(&json!("transactions"))
    );

    server
        .put("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["schedule", "stones", "checklists"] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        tabs(&server, sessions::VIEWER).await,
        json!(["stones", "schedule", "checklists"])
    );
    server
        .get("/v1/checklists")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_ok();

    server
        .delete("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        tabs(&server, sessions::VIEWER).await,
        json!(["customers", "schedule"])
    );
    server
        .get("/v1/checklists")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn a_company_limits_its_sales_reps(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    server
        .put("/v1/company/position-sections/1")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["deals", "customers"] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        tabs(&server, sessions::REP).await,
        json!(["customers", "deals"])
    );
    server
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    server
        .get("/v1/checklists")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn an_admin_keeps_every_admin_page_and_the_whole_api(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    // Ada limits her own Manager position.
    server
        .put("/v1/company/position-sections/5")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["customers", "admin_stones"] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        tabs(&server, sessions::ADMIN).await,
        json!(["customers", "admin_stones"])
    );
    for path in [
        "/v1/checklists",
        "/v1/survey-results",
        "/v1/company/position-sections",
    ] {
        server
            .get(path)
            .with_session(sessions::ADMIN)
            .await
            .assert_status_ok();
    }
}

#[test_db::test(seed)]
async fn an_admin_tab_opens_its_admin_page_without_the_admin_switch(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    server
        .get("/v1/survey-results")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .put("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["customers", "admin_surveys"] }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    server
        .get("/v1/survey-results")
        .with_session(sessions::VIEWER)
        .await
        .assert_status_ok();
    // Only that page's data: other admin routes stay closed.
    server
        .get("/v1/users/activity")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn only_managers_change_position_access_and_only_known_tabs(pool: MySqlPool) {
    seat(&pool, 13).await;
    let server = server(pool);
    for session in [sessions::REP, sessions::VIEWER] {
        server
            .put("/v1/company/position-sections/13")
            .with_session(session)
            .json(&json!({ "sections": ["stones"] }))
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    server
        .put("/v1/company/position-sections/13")
        .with_session(sessions::ADMIN)
        .json(&json!({ "sections": ["nope"] }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Shop Worker and Super Admin can't be changed.
    for position in [4, 9] {
        server
            .put(&format!("/v1/company/position-sections/{position}"))
            .with_session(sessions::ADMIN)
            .json(&json!({ "sections": [] }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}
