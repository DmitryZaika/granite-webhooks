use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: sale 7010 (Alice Lead, by Rita) with slab 6071 "Slab 2" of Taj Mahal
// (Kitchen, 30.25 sq ft; sinks Composite Black x2 and Bar Prep Sink), sale
// 7011 (Zoë, by Mark) with slab 6073; 7012 has no customer and is not listed.

fn row(rows: &Value, id: i64) -> &Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()
}

fn ids(rows: &Value) -> Vec<i64> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect()
}

#[test_db::test(seed)]
async fn the_list_shows_sales_with_their_cut_state(pool: MySqlPool) {
    sqlx::query("UPDATE slab_inventory SET cut_date = '2026-05-01 10:00:00' WHERE id = 6073")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    let rows: Value = server
        .get("/v1/transactions")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&rows), [7011, 7010]);
    let alice = row(&rows, 7010);
    assert_eq!(alice["customer_name"], "Alice Lead");
    assert_eq!(alice["seller_name"], "Rita Rep");
    assert_eq!(alice["stone_name"], "Taj Mahal:ACTIVE");
    assert_eq!(alice["bundle_with_cut"], "Slab 2:UNCUT:ACTIVE");
    assert_eq!(alice["sf"], 55.5);
    assert_eq!(alice["status"], "Sold");
    assert_eq!(alice["total_slabs"], 1);
    let sinks = alice["sink_type"].as_str().unwrap();
    assert_eq!(sinks.matches("Composite Black").count(), 2);
    assert!(sinks.contains(", "));
    assert_eq!(row(&rows, 7011)["status"], "Cut");

    let shop: Value = server
        .get("/v1/transactions?view=shop")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(row(&shop, 7010)["sf"], 30.25);
    let search: Value = server
        .get("/v1/transactions?search=alice&status=in_progress")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&search), [7010]);
    let by_rep: Value = server
        .get("/v1/transactions?sales_rep=Mark%20Manager")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(ids(&by_rep), [7011]);
    let reps: Value = server
        .get("/v1/transactions/sales-reps")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(reps, json!(["Mark Manager", "Rita Rep"]));

    server
        .get("/v1/transactions?view=admin")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let admin: Value = server
        .get("/v1/transactions?view=admin&search=nothing")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(ids(&admin), [7011, 7010]);
    assert!(
        !row(&admin, 7010)["sink_type"]
            .as_str()
            .unwrap()
            .contains(", ")
    );
    let other: Value = server
        .get("/v1/transactions")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert!(!ids(&other).contains(&7010));
}

#[test_db::test(seed)]
async fn sales_are_installed_once_every_slab_is_cut(pool: MySqlPool) {
    let server = server(pool.clone());
    let install = |id: i64, body: Value| {
        let request = server
            .post(&format!("/v1/transactions/{id}/installed"))
            .with_session(sessions::REP)
            .json(&body);
        async move { request.await }
    };
    let res = install(
        7010,
        json!({ "installed_date": "2026-06-01", "paid": true }),
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json::<Value>()["error"],
        "Cannot mark as installed - not all slabs are cut"
    );
    sqlx::query("UPDATE slab_inventory SET cut_date = NOW() WHERE sale_id = 7010")
        .execute(&pool)
        .await
        .unwrap();
    install(
        7010,
        json!({ "installed_date": "2026-06-01", "paid": true }),
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    let installed: String = sqlx::query_scalar(
        "SELECT DATE_FORMAT(installed_date, '%Y-%m-%d') FROM sales WHERE id = 7010",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(installed, "2026-06-01");
    install(7010, json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    install(7012, json!({}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .post("/v1/transactions/7010/installed")
        .with_session(sessions::OTHER_COMPANY_REP)
        .json(&json!({}))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
