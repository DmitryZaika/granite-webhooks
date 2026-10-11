mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: faucet types 3200-3205 (3205 belongs to company 101), units 6200+.

async fn list(pool: MySqlPool, query: &str) -> Value {
    let res = server(pool)
        .get(&format!("/v1/faucets{query}"))
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    res.json()
}

fn row(rows: &Value, id: i64) -> &Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("faucet {id} missing"))
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn lists_in_stock_faucets_sorted_by_name(pool: MySqlPool) {
    let rows = list(pool, "").await;
    // Bridge Brass (regular stock, no units), Hidden Faucet (not displayed,
    // still listed), Pull-Down Chrome.
    assert_eq!(ids(&rows), vec![3201, 3202, 3200]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn faucet_has_every_field(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    // 4 units: 2 free, 1 on a slab, 1 deleted.
    assert_eq!(
        row(&rows, 3200),
        &json!({
            "id": 3200,
            "name": "Pull-Down Chrome",
            "type": "single handle",
            "url": "https://img.example/pulldown.jpg",
            "supplier_id": 300,
            "is_display": true,
            "regular_stock": false,
            "retail_price": "129.99",
            "cost": "60.00",
            "available": 2
        })
    );
    let bridge = row(&rows, 3201);
    assert_eq!(bridge["regular_stock"], true);
    assert_eq!(bridge["available"], 0);
    assert_eq!(bridge["retail_price"], "0.00");
    assert_eq!(row(&rows, 3202)["is_display"], false);
    assert_eq!(row(&rows, 3203)["retail_price"], Value::Null);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn filters(pool: MySqlPool) {
    let server = server(pool);
    let cases: &[(&str, &[i64])] = &[
        ("?include_sold_out=true", &[3201, 3202, 3203, 3200]),
        ("?type=double%20handle", &[3201]),
        ("?type=double%20handle&include_sold_out=true", &[3201, 3203]),
        (
            "?type=single%20handle&type=double%20handle",
            &[3201, 3202, 3200],
        ),
        ("?supplier_id=301", &[3201]),
        ("?supplier_id=300&type=single%20handle", &[3202, 3200]),
        ("?type=wall%20mount", &[]),
    ];
    for (query, expected) in cases {
        let res = server
            .get(&format!("/v1/faucets{query}"))
            .with_session(sessions::REP)
            .await;
        res.assert_status_ok();
        assert_eq!(ids(&res.json()), *expected, "{query}");
    }
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn never_returns_deleted_or_foreign_faucets(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    let ids = ids(&rows);
    assert!(!ids.contains(&3204));
    assert!(!ids.contains(&3205));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn malformed_filters_are_400(pool: MySqlPool) {
    server(pool)
        .get("/v1/faucets?include_sold_out=1")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn tenants_are_isolated(pool: MySqlPool) {
    let rows: Value = server(pool)
        .get("/v1/faucets?include_sold_out=true")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![3205]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn requires_an_employee_or_admin(pool: MySqlPool) {
    let server = server(pool);
    server.get("/v1/faucets").await.assert_status_unauthorized();
    server
        .get("/v1/faucets")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/faucets")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}
