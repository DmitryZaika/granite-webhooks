use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: company 100's customer 1000.

async fn quote(pool: &MySqlPool, id: i32, company_id: i32) {
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep,
                             company_id, salesperson_id, created_by, status)
         VALUES (?, 1000, 'Kitchen', 'countertops', NOW(), 'Rep', ?, 100, 100, 'converted')",
    )
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await
    .unwrap();
}

fn link() -> Value {
    json!({
        "option_index": 1,
        "kind": "qbo",
        "invoice_ids": ["145", "146"],
        "detail": { "layout": [{ "label": "Deposit", "shares": { "stone": 50 } }] },
        "source_hash": "abc123",
        "enabled": true
    })
}

#[test_db::test(seed)]
async fn links_an_invoice_and_replaces_it(pool: MySqlPool) {
    quote(&pool, 7001, 100).await;
    let server = server(pool);
    let path = "/v1/quotes/7001/invoice-sync";
    let none: Value = server.get(path).with_session(sessions::REP).await.json();
    assert_eq!(none, Value::Null);

    server
        .put(path)
        .with_session(sessions::REP)
        .json(&link())
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let saved: Value = server.get(path).with_session(sessions::REP).await.json();
    assert_eq!(saved, link());

    // After a sync: the new hash; Stop syncing: enabled off.
    let mut next = link();
    next["source_hash"] = json!("def456");
    next["enabled"] = json!(false);
    server
        .put(path)
        .with_session(sessions::REP)
        .json(&next)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let saved: Value = server.get(path).with_session(sessions::REP).await.json();
    assert_eq!(saved, next);
}

#[test_db::test(seed)]
async fn refuses_bad_links_and_other_companies_quotes(pool: MySqlPool) {
    quote(&pool, 7002, 100).await;
    quote(&pool, 7003, 101).await;
    let server = server(pool);
    for (field, value) in [
        ("kind", json!("xero")),
        ("invoice_ids", json!([])),
        ("invoice_ids", json!(["1 OR 1=1"])),
        ("source_hash", json!("")),
        ("option_index", json!(-1)),
    ] {
        let mut body = link();
        body[field] = value;
        server
            .put("/v1/quotes/7002/invoice-sync")
            .with_session(sessions::REP)
            .json(&body)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
    // An own invoice is one invoice.
    let mut own = link();
    own["kind"] = json!("own");
    server
        .put("/v1/quotes/7002/invoice-sync")
        .with_session(sessions::REP)
        .json(&own)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    server
        .put("/v1/quotes/7003/invoice-sync")
        .with_session(sessions::REP)
        .json(&link())
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/quotes/7003/invoice-sync")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
