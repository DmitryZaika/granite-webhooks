mod common;

use axum::http::StatusCode;
use common::{WithSession, ids, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: sink types 3100-3107 (3106 belongs to company 101), units 6100+.

async fn list(pool: MySqlPool, query: &str) -> Value {
    let res = server(pool)
        .get(&format!("/v1/sinks{query}"))
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
        .unwrap_or_else(|| panic!("sink {id} missing"))
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn lists_in_stock_sinks_sorted_by_name(pool: MySqlPool) {
    let rows = list(pool, "").await;
    // Bar Prep Sink, Composite Black (not displayed, still listed), Dual Bowl 16,
    // Farmhouse 33 (regular stock, no units), Undermount 3219.
    assert_eq!(ids(&rows), vec![3107, 3102, 3105, 3101, 3100]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn sink_has_every_field(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    // 4 units: 2 free, 1 on a slab, 1 deleted.
    assert_eq!(
        row(&rows, 3100),
        &json!({
            "id": 3100,
            "name": "Undermount 3219",
            "type": "stainless 18 gauge",
            "url": "https://img.example/undermount.jpg",
            "supplier_id": 300,
            "is_display": true,
            "regular_stock": false,
            "length": 32,
            "width": 19,
            "retail_price": "199.00",
            "cost": "90.00",
            "available": 2
        })
    );
    let farmhouse = row(&rows, 3101);
    assert_eq!(farmhouse["regular_stock"], true);
    assert_eq!(farmhouse["available"], 0);
    assert_eq!(farmhouse["retail_price"], "0.00");
    assert_eq!(farmhouse["cost"], Value::Null);
    assert_eq!(row(&rows, 3102)["is_display"], false);
    let ceramic = row(&rows, 3103);
    assert_eq!(ceramic["retail_price"], Value::Null);
    assert_eq!(ceramic["available"], 0);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn filters(pool: MySqlPool) {
    let server = server(pool);
    let cases: &[(&str, &[i64])] = &[
        (
            "?include_sold_out=true",
            &[3107, 3103, 3102, 3105, 3101, 3100],
        ),
        ("?type=composite", &[3102]),
        ("?type=composite&type=ceramic", &[3102]),
        (
            "?type=composite&type=ceramic&include_sold_out=true",
            &[3103, 3102],
        ),
        ("?type=stainless%2018%20gauge", &[3100]),
        ("?type=stainless+18+gauge", &[3100]),
        ("?supplier_id=301", &[3105, 3101]),
        ("?supplier_id=301&type=farm%20house", &[3101]),
        ("?type=bar%20sink", &[3107]),
        ("?type=granite", &[]),
    ];
    for (query, expected) in cases {
        let res = server
            .get(&format!("/v1/sinks{query}"))
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
async fn never_returns_deleted_or_foreign_sinks(pool: MySqlPool) {
    let rows = list(pool, "?include_sold_out=true").await;
    let ids = ids(&rows);
    assert!(!ids.contains(&3104));
    assert!(!ids.contains(&3106));
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn malformed_filters_are_400(pool: MySqlPool) {
    server(pool)
        .get("/v1/sinks?supplier_id=abc")
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
        .get("/v1/sinks?include_sold_out=true")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json();
    assert_eq!(ids(&rows), vec![3106]);
}

#[sqlx::test(
    migrations = "../migrations",
    fixtures(path = "../seed", scripts("local_seed"))
)]
async fn requires_an_employee_or_admin(pool: MySqlPool) {
    let server = server(pool);
    server.get("/v1/sinks").await.assert_status_unauthorized();
    server
        .get("/v1/sinks")
        .with_session(sessions::VIEWER)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/sinks")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
}
