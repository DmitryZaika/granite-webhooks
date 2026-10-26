//! The company's current offer (`/v1/followups/offer`) and the rep's text
//! sign-off (`/v1/followups/signature`) for AI follow-ups.

use crate::common;

use axum::http::StatusCode;
use common::{WithSession, server, sessions};
use serde_json::{Value, json};
use sqlx::MySqlPool;

type OfferRow = (
    String,
    Option<chrono::NaiveDate>,
    Option<i32>,
    chrono::NaiveDateTime,
);

async fn offer_row(pool: &MySqlPool, company_id: i32) -> Option<OfferRow> {
    sqlx::query_as(
        "SELECT offer, ends_on, updated_by, CAST(updated_at AS DATETIME) FROM ai_followup_offers WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn signature_of(pool: &MySqlPool, user_id: i32) -> Option<String> {
    sqlx::query_scalar("SELECT followup_sms_signature FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn the_offer_is_saved_per_company_and_cleared_with_its_last_day(pool: MySqlPool) {
    let server = server(pool.clone());
    let empty = server
        .get("/v1/followups/offer")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(empty, json!({ "offer": "", "ends_on": null }));

    let saved = server
        .put("/v1/followups/offer")
        .with_session(sessions::REP)
        .json(&json!({ "offer": "  10% off quartz installs booked this month ", "ends_on": "2026-10-31" }))
        .await
        .json::<Value>();
    let offer =
        json!({ "offer": "10% off quartz installs booked this month", "ends_on": "2026-10-31" });
    assert_eq!(saved, offer);
    let got = server
        .get("/v1/followups/offer")
        .with_session(sessions::MANAGER)
        .await
        .json::<Value>();
    assert_eq!(got, offer);
    let other = server
        .get("/v1/followups/offer")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .json::<Value>();
    assert_eq!(other, json!({ "offer": "", "ends_on": null }));

    // Clearing keeps the row (its time tells running follow-ups) and drops
    // the last day.
    let cleared = server
        .put("/v1/followups/offer")
        .with_session(sessions::MANAGER)
        .json(&json!({ "offer": "   ", "ends_on": "2026-11-30" }))
        .await
        .json::<Value>();
    assert_eq!(cleared, json!({ "offer": "", "ends_on": null }));
    let row = offer_row(&pool, 100).await.unwrap();
    assert_eq!((row.0.as_str(), row.1, row.2), ("", None, Some(101)));
    assert!(offer_row(&pool, 101).await.is_none());
}

#[test_db::test(seed)]
async fn saving_the_same_offer_again_keeps_its_time(pool: MySqlPool) {
    sqlx::query(
        "INSERT INTO ai_followup_offers (company_id, offer, ends_on, updated_by, updated_at)
         VALUES (100, 'Free sink with any kitchen', NULL, 100, '2026-10-01 09:00:00')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let server = server(pool.clone());
    server
        .put("/v1/followups/offer")
        .with_session(sessions::MANAGER)
        .json(&json!({ "offer": "Free sink with any kitchen", "ends_on": null }))
        .await
        .assert_status_ok();
    let row = offer_row(&pool, 100).await.unwrap();
    assert_eq!(row.2, Some(100));
    assert_eq!(row.3.to_string(), "2026-10-01 09:00:00");

    server
        .put("/v1/followups/offer")
        .with_session(sessions::MANAGER)
        .json(&json!({ "offer": "Free sink with any kitchen", "ends_on": "2026-12-31" }))
        .await
        .assert_status_ok();
    let row = offer_row(&pool, 100).await.unwrap();
    assert_eq!(row.2, Some(101));
    assert!(row.3.to_string().as_str() > "2026-10-01 09:00:00");
}

#[test_db::test(seed)]
async fn the_signature_is_the_callers_own(pool: MySqlPool) {
    let server = server(pool.clone());
    let saved = server
        .put("/v1/followups/signature")
        .with_session(sessions::REP)
        .json(&json!({ "signature": "\n Rita  \r\nAcme Stone \n\n" }))
        .await
        .json::<Value>();
    assert_eq!(saved, json!({ "signature": "Rita\nAcme Stone" }));
    assert_eq!(
        signature_of(&pool, 100).await.as_deref(),
        Some("Rita\nAcme Stone")
    );
    let mine = server
        .get("/v1/followups/signature")
        .with_session(sessions::REP)
        .await
        .json::<Value>();
    assert_eq!(mine, saved);
    let theirs = server
        .get("/v1/followups/signature")
        .with_session(sessions::MANAGER)
        .await
        .json::<Value>();
    assert_eq!(theirs, json!({ "signature": "" }));

    server
        .put("/v1/followups/signature")
        .with_session(sessions::REP)
        .json(&json!({ "signature": "  " }))
        .await
        .assert_status_ok();
    assert_eq!(signature_of(&pool, 100).await, None);
}

#[test_db::test(seed)]
async fn too_long_is_refused_and_a_session_is_needed(pool: MySqlPool) {
    let server = server(pool.clone());
    server
        .put("/v1/followups/offer")
        .with_session(sessions::REP)
        .json(&json!({ "offer": "x".repeat(501), "ends_on": null }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    server
        .put("/v1/followups/signature")
        .with_session(sessions::REP)
        .json(&json!({ "signature": "é".repeat(201) }))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert!(offer_row(&pool, 100).await.is_none());
    assert_eq!(signature_of(&pool, 100).await, None);
    for path in ["/v1/followups/offer", "/v1/followups/signature"] {
        server
            .get(path)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        server
            .get(path)
            .with_session(sessions::VIEWER)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}
