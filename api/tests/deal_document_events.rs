mod common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::Value;
use sqlx::MySqlPool;

#[test_db::test(seed)]
async fn a_deal_lists_its_quote_and_contract_events(pool: MySqlPool) {
    sqlx::raw_sql(
        "INSERT INTO quotes (id, customer_id, quote_name, quote_type, created_date, sales_rep, company_id, deal_id)
           VALUES (9901, 1000, 'Smith: Kitchen', 'kitchen', NOW(), 'Rita', 100, 8000),
                  (9902, 1000, 'Other deal', 'kitchen', NOW(), 'Rita', 100, 8004);
         INSERT INTO quote_emails (id, quote_id, revision_number, token, recipients, subject, sent_by, sent_at)
           VALUES (9911, 9901, 1, 'tok-9911', 'alice@example.com', 'Your quote', 100, '2026-05-01 14:30:00'),
                  (9912, 9902, 1, 'tok-9912', 'alice@example.com', 'Not this deal', 100, NOW());
         INSERT INTO signature_requests (id, company_id, customer_id, deal_id, created_by_user_id, filename, file_url,
                                         signer_name, signer_email, status, created_at, updated_at)
           VALUES (9921, 100, 1000, NULL, 100, 'Contract - Smith  Kitchen.pdf', 'x', 'Alice', 'a@x.test', 'signed',
                   '2026-05-02 10:00:00', '2026-05-03 09:00:00'),
                  (9922, 100, 1000, NULL, 100, 'Unrelated.pdf', 'x', 'Alice', 'a@x.test', 'sent', NOW(), NOW()),
                  (9923, 100, 1000, 8000, 100, 'Draft.pdf', 'x', 'Alice', 'a@x.test', 'draft', NOW(), NOW());",
    )
    .execute(&pool)
    .await
    .unwrap();

    let res = server(pool)
        .get("/v1/deals/8000/document-events")
        .with_session(sessions::REP)
        .await;
    res.assert_status_ok();
    let events: Vec<Value> = res.json();
    let ids: Vec<&str> = events.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        ["quote-email-9911", "contract-sent-9921", "contract-signed-9921"]
    );
    assert_eq!(events[0]["kind"], "quote_emailed");
    assert_eq!(events[0]["title"], "Smith: Kitchen");
    assert_eq!(events[0]["detail"], "alice@example.com");
    assert_eq!(events[0]["at"], "2026-05-01T14:30:00Z");
    assert_eq!(events[0]["quote_id"], 9901);
    assert_eq!(events[1]["title"], "Contract - Smith  Kitchen");
    assert_eq!(events[1]["detail"], "Alice");
    assert_eq!(events[2]["kind"], "contract_signed");
    assert_eq!(events[2]["at"], "2026-05-03T09:00:00Z");
    assert_eq!(events[2]["by"], Value::Null);
}

#[test_db::test(seed)]
async fn another_companys_deal_is_not_found(pool: MySqlPool) {
    server(pool)
        .get("/v1/deals/8000/document-events")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::NOT_FOUND);
}
