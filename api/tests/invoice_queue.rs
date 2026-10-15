mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

// Seed sales 7010 (Alice, by Rita) and 7011 (Zoë, by Mark); 7012 has no
// customer. None has a QuickBooks invoice.

#[test_db::test(seed)]
async fn admins_see_sales_waiting_for_an_invoice(pool: MySqlPool) {
    sqlx::query("UPDATE sales SET qbo_id = 55 WHERE id = 7011")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(pool);
    let rows: Value = server
        .get("/v1/invoice-queue")
        .with_session(sessions::ADMIN)
        .await
        .json();
    let ids: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .filter(|id| (7010..=7012).contains(id))
        .collect();
    assert_eq!(ids, [7010]);
    let one = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == 7010)
        .unwrap();
    assert_eq!(one["customer_name"], "Alice Lead");
    assert_eq!(one["seller_name"], "Rita Rep");
    assert_eq!(one["sale_date"], "2026-04-15T15:00:00.000Z");

    let detail: Value = server
        .get("/v1/invoice-queue/7010")
        .with_session(sessions::ADMIN)
        .await
        .json();
    assert_eq!(detail["phone"], "317-555-0100");
    assert_eq!(detail["qbo_id"], Value::Null);
    server
        .get("/v1/invoice-queue/7010")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    server
        .get("/v1/invoice-queue/7010")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
