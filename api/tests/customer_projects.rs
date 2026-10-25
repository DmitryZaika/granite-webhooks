use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: Erin Builder (1004) is the parent of Frank Child (1005). Erin has
// sales 7000 and 7001, Frank 7002; Erin's newest deal 8601 is in "New".
// Alice (1000) has no family. Company 101's customer 2000 is out of reach.

async fn projects(pool: &MySqlPool, id: i32, session: &str) -> axum_test::TestResponse {
    server(pool.clone())
        .get(&format!("/v1/customers/{id}/projects"))
        .with_session(session)
        .await
}

fn names(body: &Value) -> Vec<&str> {
    body["customers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn a_child_shows_its_parent_family(pool: MySqlPool) {
    for id in [1004, 1005] {
        let res = projects(&pool, id, sessions::REP).await;
        res.assert_status_ok();
        let body: Value = res.json();
        assert_eq!(body["parent_id"], 1004);
        assert_eq!(names(&body), ["Erin Builder", "Frank Child"]);
    }
    let body: Value = projects(&pool, 1005, sessions::REP).await.json();
    let erin = &body["customers"][0];
    assert_eq!(erin["deal_stage"], "New");
    assert_eq!(erin["company_name"], "Erin Builders LLC");
    let sales: Vec<i64> = erin["sales"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_i64().unwrap())
        .collect();
    assert_eq!(sales, [7000, 7001]);
    assert_eq!(erin["sales"][0]["price"], "1000.50");
    assert_eq!(body["customers"][1]["sales"][0]["id"], 7002);
}

#[test_db::test(seed)]
async fn linking_moves_a_customer_into_a_family(pool: MySqlPool) {
    let body: Value = projects(&pool, 1000, sessions::REP).await.json();
    assert_eq!(names(&body), ["Alice Lead"]);
    assert_eq!(body["customers"][0]["emails"][0], "alice@example.com");

    let server = server(pool.clone());
    server
        .put("/v1/customers/1000/parent")
        .with_session(sessions::REP)
        .json(&json!({ "parent_id": 1004 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let body: Value = projects(&pool, 1004, sessions::REP).await.json();
    assert_eq!(names(&body), ["Alice Lead", "Erin Builder", "Frank Child"]);

    server
        .put("/v1/customers/1000/parent")
        .with_session(sessions::REP)
        .json(&json!({ "parent_id": 1000 }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/customers/1000/parent")
        .with_session(sessions::REP)
        .json(&json!({ "parent_id": 2000 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn other_companies_customers_are_out_of_reach(pool: MySqlPool) {
    projects(&pool, 2000, sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server(pool)
        .put("/v1/customers/2000/parent")
        .with_session(sessions::REP)
        .json(&json!({ "parent_id": 1004 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

#[test_db::test(seed)]
async fn a_customers_sales_list_their_sellers(pool: MySqlPool) {
    let server = server(pool);
    let res = server
        .get("/v1/customers/1000/sales")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let sales: Value = res.json();
    assert_eq!(sales[0]["id"], 7010);
    assert_eq!(sales[0]["price"], "4200.00");
    assert_eq!(sales[0]["seller_name"], "Rita Rep");
    server
        .get("/v1/customers/2000/sales")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
