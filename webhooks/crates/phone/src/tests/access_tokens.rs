//! The access tokens on this crate's routes (the extractors are tested in
//! `webhooks_core::axum_helpers::access_token`).

use crate::tests::utils::new_test_app;
use axum::http::StatusCode;
use common::access_token::{self as tokens, NewAccessToken, Scope};
use sqlx::MySqlPool;

async fn token(pool: &MySqlPool, company_id: i32, scopes: &[Scope]) -> String {
    sqlx::query("INSERT IGNORE INTO company (id, name) VALUES (?, 'Token Co')")
        .bind(company_id)
        .execute(pool)
        .await
        .unwrap();
    let (_, token) = tokens::create(
        pool,
        &NewAccessToken {
            name: "route test",
            company_id: Some(company_id),
            scopes,
            expires_at: None,
            created_by: None,
        },
    )
    .await
    .unwrap();
    token
}

fn cloudtalk_sms() -> serde_json::Value {
    serde_json::json!({
        "id": 2_200_000_901_i64,
        "sender": "+16468956758",
        "recipient": "+13173161456",
        "text": "sent with an access token",
        "agent": "540273"
    })
}

async fn cloudtalk_sms_count(pool: &MySqlPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM cloudtalk_sms")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test]
async fn cloudtalk_accepts_its_companys_telephony_token(pool: MySqlPool) {
    let own = token(&pool, 42, &[Scope::Telephony]).await;
    let other = token(&pool, 43, &[Scope::Telephony]).await;
    let app = new_test_app(pool.clone());

    let refused = app
        .post("/cloudtalk/sms/42")
        .authorization_bearer(&other)
        .json(&cloudtalk_sms())
        .await;
    assert_eq!(refused.status_code(), StatusCode::FORBIDDEN);
    assert_eq!(cloudtalk_sms_count(&pool).await, 0);

    let accepted = app
        .post("/cloudtalk/sms/42")
        .authorization_bearer(&own)
        .json(&cloudtalk_sms())
        .await;
    assert_eq!(accepted.status_code(), StatusCode::OK);
    assert_eq!(cloudtalk_sms_count(&pool).await, 1);
}

#[test_db::test]
async fn ringcentral_notify_takes_the_token_as_its_verification_token(pool: MySqlPool) {
    let own = token(&pool, 44, &[Scope::Telephony]).await;
    let app = new_test_app(pool);
    let body: serde_json::Value =
        serde_json::from_slice(crate::tests::ringcentral::NOTIFY_INBOUND_SMS).unwrap();

    let response = app
        .post("/ringcentral/notify/44")
        .add_header("Verification-Token", own.as_str())
        .json(&body)
        .await;
    assert_ne!(response.status_code(), StatusCode::FORBIDDEN);

    let handshake = app
        .post("/ringcentral/notify/44")
        .add_header("Validation-Token", "abc-123")
        .await;
    assert_eq!(handshake.status_code(), StatusCode::OK);
    assert_eq!(handshake.header("Validation-Token"), "abc-123");
}
