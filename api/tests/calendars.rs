mod common;

use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn built_in_calendars_are_created_and_listed_first(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug, is_hidden) VALUES
           (8700, 100, 'Zeta', 'pink', NULL, 0),
           (8701, 100, 'Alpha', 'teal', NULL, 0),
           (8702, 100, 'Hidden for all', 'gray', NULL, 1),
           (8703, 100, 'Hidden for Rita', 'gray', NULL, 0),
           (8704, 101, 'Other company', 'gray', NULL, 0);
         INSERT INTO calendar_hidden_users (calendar_id, user_id, company_id) VALUES (8703, 100, 100);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let list: Value = server(pool.clone())
        .get("/v1/calendars")
        .with_session(sessions::REP)
        .await
        .json();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Templates", "Installations", "Estimates", "Alpha", "Zeta"]
    );
    assert_eq!(list[2]["slug"], "estimates");
    assert_eq!(list[2]["color"], "blue");
    assert_eq!(list[2]["is_system"], true);
    assert_eq!(
        list[3],
        json!({ "id": 8701, "name": "Alpha", "color": "teal", "slug": null, "is_system": false })
    );
    // Calling again creates nothing more.
    server(pool.clone())
        .get("/v1/calendars")
        .with_session(sessions::REP)
        .await
        .assert_status_ok();
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM calendars WHERE company_id = 100 AND slug IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 3);
    // Mark sees the calendar hidden from Rita.
    let mark: Value = server(pool.clone())
        .get("/v1/calendars")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert!(
        mark.as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "Hidden for Rita")
    );
    server(pool.clone())
        .get("/v1/calendars")
        .await
        .assert_status_unauthorized();
    server(pool)
        .get("/v1/calendars")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

#[test_db::test(seed)]
async fn renamed_built_in_calendar_gets_its_name_back(pool: MySqlPool) {
    sqlx::query("INSERT INTO calendars (company_id, name, color, slug) VALUES (100, 'My estimates', 'purple', 'estimates')")
        .execute(&pool)
        .await
        .unwrap();
    let list: Value = server(pool)
        .get("/v1/calendars")
        .with_session(sessions::REP)
        .await
        .json();
    let estimates = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["slug"] == "estimates")
        .unwrap();
    assert_eq!(estimates["name"], "Estimates");
    assert_eq!(estimates["color"], "purple");
}
