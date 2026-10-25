//! `GET /v1/followups/timeline`: the conversation the AI follow-up writer
//! reads (seed plus `fixtures/followups/gather.sql`).

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const FIXTURE: &str = include_str!("fixtures/followups/gather.sql");

#[test_db::test(seed)]
async fn the_conversation_is_both_providers_and_email_oldest_first(pool: MySqlPool) {
    sqlx::raw_sql(FIXTURE).execute(&pool).await.unwrap();
    let server = server(pool);
    let body: Value = server
        .get("/v1/followups/timeline?customer_id=1000")
        .with_session(sessions::REP)
        .await
        .json();
    let timeline = body["timeline"].as_array().unwrap();
    assert_eq!(
        timeline[..3],
        [
            json!({ "at": "2026-06-01 12:00", "channel": "sms", "direction": "inbound", "text": "Old RingCentral text from the second phone" }),
            json!({ "at": "2026-09-10 10:00", "channel": "email", "direction": "inbound", "text": "Subject: Kitchen quote\nCould you quote the island?" }),
            json!({ "at": "2026-09-11 10:00", "channel": "email", "direction": "outbound", "text": "Subject: Re: Kitchen quote\nAttached is the quote." }),
        ]
    );
    let by_phone: Value = server
        .get("/v1/followups/timeline?phone_digits=3175550100")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(by_phone, body);
    // Another company's customer is not found.
    server
        .get("/v1/followups/timeline?customer_id=1000")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status_not_found();
}

#[test_db::test(seed)]
async fn a_customer_or_a_valid_phone_is_required(pool: MySqlPool) {
    let server = server(pool);
    for (query, error) in [
        ("", "customer_or_phone_required"),
        ("?customer_id=0", "customer_or_phone_required"),
        ("?phone_digits=12ab", "invalid_phone"),
    ] {
        let response = server
            .get(&format!("/v1/followups/timeline{query}"))
            .with_session(sessions::REP)
            .await;
        response.assert_status_bad_request();
        assert_eq!(response.json::<Value>()["error"], error);
    }
    server
        .get("/v1/followups/timeline?customer_id=1000")
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}
