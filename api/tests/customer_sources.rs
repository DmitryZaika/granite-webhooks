mod common;

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
