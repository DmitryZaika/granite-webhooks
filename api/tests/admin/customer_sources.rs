use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn labels(server: &axum_test::TestServer) -> Vec<String> {
    server
        .get("/v1/customer-sources")
        .with_session(sessions::REP)
        .await
        .json::<Value>()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["label"].as_str().unwrap().to_string())
        .collect()
}

#[test_db::test(seed)]
async fn admins_manage_sources(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO customer_sources (id, company_id, source_key, label, is_system) VALUES
           (900, 100, 'check-in', 'Walk-in', 1), (901, 101, 'check-in', 'Walk-in', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE customers SET source = 'Yelp' WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool.clone());
    let create = |session: &'static str, name: &str| {
        let request = server
            .post("/v1/customer-sources")
            .with_session(session)
            .json(&json!({ "name": name }));
        async move { request.await }
    };
    create(sessions::REP, "Yelp")
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let res = create(sessions::ADMIN, " Yelp ").await;
    res.assert_status(StatusCode::CREATED);
    let yelp = res.json::<Value>()["id"].as_i64().unwrap();
    assert_eq!(labels(&server).await, ["Walk-in", "Yelp"]);
    // A customer can be added with the company's own source.
    server
        .post("/v1/customers")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Yelp lead", "source": "Yelp" }))
        .await
        .assert_status(StatusCode::CREATED);
    for taken in ["yelp", "walk-in", "Leads"] {
        create(sessions::ADMIN, taken)
            .await
            .assert_status(StatusCode::CONFLICT);
    }
    create(sessions::ADMIN, " ")
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // Renaming a custom source moves its customers along.
    server
        .put(&format!("/v1/customer-sources/{yelp}"))
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Google" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let source: String = sqlx::query_scalar("SELECT source FROM customers WHERE id = 1000")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(source, "Google");
    // A built-in one keeps its key.
    server
        .put("/v1/customer-sources/900")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Showroom" }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let key: String = sqlx::query_scalar("SELECT source_key FROM customer_sources WHERE id = 900")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(key, "check-in");
    server
        .put("/v1/customer-sources/901")
        .with_session(sessions::ADMIN)
        .json(&json!({ "name": "Mine" }))
        .await
        .assert_status(StatusCode::NOT_FOUND);

    server
        .delete("/v1/customer-sources/900")
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .delete(&format!("/v1/customer-sources/{yelp}"))
        .with_session(sessions::ADMIN)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(labels(&server).await, ["Showroom"]);
}

#[test_db::test(seed)]
async fn a_source_ticked_as_walk_in_shows_under_walk_ins(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO customer_sources (id, company_id, source_key, label, is_system) VALUES
           (900, 100, 'check-in', 'Walk-in - Lexington', 1),
           (902, 100, 'Walk-in - Nicholasville', 'Walk-in - Nicholasville', 0),
           (903, 101, 'Theirs', 'Theirs', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE customers SET source = 'Walk-in - Nicholasville' WHERE id = 1000")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool.clone());
    let walk_in_ids = || async {
        server
            .get("/v1/customers/page?tab=walkin&page_size=100")
            .with_session(sessions::ADMIN)
            .await
            .json::<Value>()["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    let tick = |id: i64, session: &'static str, on: bool| {
        let request = server
            .put(&format!("/v1/customer-sources/{id}/walk-in"))
            .with_session(session)
            .json(&json!({ "walk_in": on }));
        async move { request.await }
    };
    assert!(!walk_in_ids().await.contains(&1000));

    tick(902, sessions::REP, true)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    tick(900, sessions::ADMIN, false)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    tick(903, sessions::ADMIN, true)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    tick(902, sessions::ADMIN, true)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(walk_in_ids().await.contains(&1000));
    let listed = server
        .get("/v1/customer-sources")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let nich = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == 902)
        .unwrap()
        .clone();
    assert_eq!(nich["is_walkin"], true);

    tick(902, sessions::ADMIN, false)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert!(!walk_in_ids().await.contains(&1000));
}
