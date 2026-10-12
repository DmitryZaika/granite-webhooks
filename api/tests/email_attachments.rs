mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

// Seed: attachment 8810 "sketch.png" (https://example.test/sketch.png) is on
// Olga's message 8802 (company 101). Olga's deal 8009 belongs to customer
// 2000 (buyer@other-customer.test). Dina (company 100) owns deal 8600.

const OLGA: &str = sessions::OTHER_COMPANY_REP;
const DINA: &str = sessions::DEALER;

async fn urls(server: &axum_test::TestServer, session: &str, query: &str) -> Value {
    let res = server
        .get(&format!("/v1/deals/file-urls?{query}"))
        .with_session(session)
        .await;
    res.assert_status_ok();
    res.json::<Value>()["urls"].clone()
}

#[test_db::test(seed)]
async fn an_attachment_is_saved_on_a_deal_once(pool: MySqlPool) {
    let server = server(pool.clone());
    assert!(
        !urls(&server, OLGA, "deal_id=8009")
            .await
            .as_array()
            .unwrap()
            .contains(&json!("https://example.test/sketch.png"))
    );
    for _ in 0..2 {
        server
            .post("/v1/email-attachments/8810/deal")
            .with_session(OLGA)
            .json(&json!({ "deal_id": 8009 }))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }
    let saved: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT image_url, created_by FROM deals_images WHERE deal_id = 8009 AND image_url = 'https://example.test/sketch.png'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        saved,
        [(
            "https://example.test/sketch.png".into(),
            Some("Olga Other".into())
        )]
    );
    assert!(
        urls(&server, OLGA, "deal_id=8009")
            .await
            .as_array()
            .unwrap()
            .contains(&json!("https://example.test/sketch.png"))
    );
    // By the customer's address, across their deals.
    assert!(
        urls(&server, OLGA, "customer_email=BUYER@other-customer.test")
            .await
            .as_array()
            .unwrap()
            .contains(&json!("https://example.test/sketch.png"))
    );
}

#[test_db::test(seed)]
async fn documents_go_to_the_deal_documents(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO email_attachments (id, email_id, content_type, content_subtype, filename, url)
         VALUES (8811, 8802, 'application', 'pdf', 'quote.pdf', 'https://example.test/quote.pdf')",
    )
    .execute(&pool)
    .await
    .unwrap();
    server(pool.clone())
        .post("/v1/email-attachments/8811/deal")
        .with_session(OLGA)
        .json(&json!({ "deal_id": 8009 }))
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM deals_documents WHERE deal_id = 8009 AND image_url = 'https://example.test/quote.pdf'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[test_db::test(seed)]
async fn other_companies_attachments_and_deals_are_out_of_reach(pool: MySqlPool) {
    let server = server(pool);
    // Dina may not read Olga's attachment, even onto her own deal.
    server
        .post("/v1/email-attachments/8810/deal")
        .with_session(DINA)
        .json(&json!({ "deal_id": 8600 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    // Olga may not write onto Dina's deal.
    server
        .post("/v1/email-attachments/8810/deal")
        .with_session(OLGA)
        .json(&json!({ "deal_id": 8600 }))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    server
        .get("/v1/deals/file-urls?deal_id=8009")
        .with_session(DINA)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert_eq!(
        urls(&server, DINA, "customer_email=buyer@other-customer.test").await,
        json!([])
    );
    assert_eq!(urls(&server, DINA, "").await, json!([]));
}
