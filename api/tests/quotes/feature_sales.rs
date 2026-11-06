//! Feature sales: extras on sold quotes, priced from the quote (no sync).

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

async fn sold_quote(pool: &MySqlPool) {
    let doc = json!({
        "version": 2,
        "areas": [{ "id": "a1", "name": "Kitchen", "quantity": 1 }],
        "colorOptions": [{ "id": "o1", "byArea": {} }],
        "items": [{ "id": "i1", "kind": "misc", "areaId": null, "name": "Seam Phantom", "quantity": 1, "price": 120 }],
    });
    let list = json!({
        "units": "in", "currency": "$", "expirationDays": 30, "lineItemPriceRounding": 2,
        "arrProducts": [], "arrOtherItems": [], "arrSinkModels": [],
    });
    sqlx::query(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id,
                             salesperson_id, created_by, status, draft_drawing_json, price_snapshot_json, signed_at)
         VALUES (9711, 1000, 'Kitchen', 'quote', NOW(), 'Rita Rep', 100, 100, 100, 'sold', ?, ?, '2026-10-01 12:00:00')",
    )
    .bind(doc.to_string())
    .bind(list.to_string())
    .execute(pool)
    .await
    .unwrap();
}

#[test_db::test(seed)]
async fn sold_quotes_count_their_extras_without_a_sync(pool: MySqlPool) {
    let server = server(pool.clone());
    sold_quote(&pool).await;

    let mine = server
        .get("/v1/feature-sales?from=2026-10-01&to=2026-10-31")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    let row = &mine["rows"][0];
    assert_eq!(row["quoteId"], 9711);
    assert_eq!(row["signedAt"], "2026-10-01");
    assert_eq!(row["features"], json!({ "phantom_seam": 120.0 }));

    // Outside the dates: nothing.
    let later = server
        .get("/v1/feature-sales?from=2026-11-01")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(later["rows"], json!([]));

    server
        .get("/v1/feature-sales?scope=company")
        .with_session(sessions::ADMIN)
        .await
        .assert_status_ok();
    server
        .get("/v1/feature-sales?scope=company")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::FORBIDDEN);
}
