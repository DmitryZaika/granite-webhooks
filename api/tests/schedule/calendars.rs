use crate::common;

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
        json!({ "id": 8701, "name": "Alpha", "color": "teal", "slug": null, "is_system": false, "is_hidden": false, "shows_day_map": false })
    );
    // The day map is on for Templates and Installations by default.
    let maps: Vec<bool> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["shows_day_map"].as_bool().unwrap())
        .collect();
    assert_eq!(maps, vec![true, true, false, false, false]);
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

#[test_db::test(seed)]
async fn admins_manage_calendars(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO calendars (id, company_id, name, color, slug, is_hidden) VALUES
           (8710, 100, 'Installations', 'red', 'installations', 0),
           (8711, 101, 'Theirs', 'gray', NULL, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    let status = axum::http::StatusCode::from_u16;
    server
        .post("/v1/calendars")
        .with_session(sessions::REP)
        .json(&json!({ "name": "Crew" }))
        .await
        .assert_status(status(403).unwrap());
    for (body, code) in [
        (json!({ "name": " " }), 400),
        (json!({ "name": "estimates" }), 400),
        (json!({ "name": "x".repeat(101) }), 400),
    ] {
        server
            .post("/v1/calendars")
            .with_session(sessions::ADMIN)
            .json(&body)
            .await
            .assert_status(status(code).unwrap());
    }
    let res = server
        .post("/v1/calendars")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": " Crew ", "hidden": true }))
        .await;
    res.assert_status(status(201).unwrap());
    let id = res.json::<Value>()["id"].as_i64().unwrap();
    let row = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, String, bool, bool)>(
                "SELECT name, color, is_hidden <> 0, deleted_at IS NOT NULL FROM calendars WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(row(id).await, ("Crew".into(), "blue".into(), true, false));

    server
        .put(&format!("/v1/calendars/{id}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Crew B", "color": "teal" }))
        .await
        .assert_status(status(204).unwrap());
    server
        .put(&format!("/v1/calendars/{id}/hidden"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "hidden": false }))
        .await
        .assert_status(status(204).unwrap());
    assert_eq!(
        row(id).await,
        ("Crew B".into(), "teal".into(), false, false)
    );

    // A built-in calendar keeps its name and cannot be deleted.
    server
        .put("/v1/calendars/8710")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Installs", "color": "pink" }))
        .await
        .assert_status(status(204).unwrap());
    assert_eq!(row(8710).await.0, "Installations");
    assert_eq!(row(8710).await.1, "pink");
    server
        .delete("/v1/calendars/8710")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(status(400).unwrap());
    server
        .delete("/v1/calendars/8711")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(status(404).unwrap());
    server
        .delete(&format!("/v1/calendars/{id}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(status(204).unwrap());
    assert!(row(id).await.3);
}

#[test_db::test(seed)]
async fn an_admin_turns_a_calendars_day_map_on_and_off(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO calendars (id, company_id, name, color) VALUES (8711, 100, 'Service', 'teal')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let day_map = |id: i64| {
        let pool = pool.clone();
        async move {
            let list: Value = server(pool)
                .get("/v1/calendars")
                .with_session(sessions::ADMIN)
                .await
                .json();
            list.as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == id)
                .map(|c| c["shows_day_map"].as_bool().unwrap())
                .unwrap()
        }
    };
    assert!(!day_map(8711).await);
    server(pool.clone())
        .put("/v1/calendars/8711/day-map")
        .with_session(sessions::REP)
        .json(&json!({ "shows_day_map": true }))
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
    server(pool.clone())
        .put("/v1/calendars/8711/day-map")
        .with_session(sessions::ADMIN)
        .json(&json!({ "shows_day_map": true }))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(day_map(8711).await);
    // A built-in one can be turned off.
    let installations: i64 = sqlx::query_scalar(
        "SELECT id FROM calendars WHERE company_id = 100 AND slug = 'installations'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    server(pool.clone())
        .put(&format!("/v1/calendars/{installations}/day-map"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "shows_day_map": false }))
        .await
        .assert_status(axum::http::StatusCode::NO_CONTENT);
    assert!(!day_map(installations).await);
}
