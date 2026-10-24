//! The Granite Phone app's Emails and Schedule screens call the CRM's own
//! routes (`/v1/emails`, `/v1/schedule`, `/v1/events`) with its device token
//! (`X-Granite-Device`) instead of the cookie (`auth::DEVICE_ROUTES`).
//!
//! Fixture phones use n = 301..=310.

mod common;
mod phone_common;

use axum::http::StatusCode;
use common::{WithSession, sessions};
use phone_common::{HEADER, add_session, phone_server, revoke_session, sql, token};
use serde_json::Value;
use sqlx::MySqlPool;

const UNREAD: &str = "/v1/emails/unread";
const EVENTS: &str = "/v1/schedule/events?start=2026-10-01T00:00:00Z&end=2026-11-01T00:00:00Z";

#[test_db::test(seed)]
async fn the_app_reads_emails_and_the_schedule_with_its_device_token(pool: MySqlPool) {
    // Rita (user 100, company 100) signed in on her phone.
    add_session(&pool, 301, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;

    // The same answers as her CRM session gets.
    for path in [UNREAD, EVENTS] {
        let by_phone = server.get(path).add_header(HEADER, token(301)).await;
        by_phone.assert_status_ok();
        let by_cookie = server.get(path).with_session(sessions::REP).await;
        by_cookie.assert_status_ok();
        assert_eq!(
            by_phone.json::<Value>(),
            by_cookie.json::<Value>(),
            "{path}"
        );
    }

    // Nothing without a token or cookie; a bad, revoked or idle token is out
    // even with a cookie (the header decides alone).
    server
        .get(UNREAD)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    server
        .get(UNREAD)
        .add_header(HEADER, "not-a-token")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    add_session(&pool, 302, 100, 100).await;
    revoke_session(&pool, 302).await;
    server
        .get(EVENTS)
        .add_header(HEADER, token(302))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
    add_session(&pool, 303, 100, 100).await;
    sql(
        &pool,
        "UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP() - INTERVAL 31 DAY,
                created_at = UTC_TIMESTAMP() - INTERVAL 40 DAY
          WHERE id = 950303",
    )
    .await;
    server
        .get(UNREAD)
        .add_header(HEADER, token(303))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // Elsewhere the cookie routes still need the cookie, even right next to
    // the prefixes (`/v1/email-attachments` is not under `/v1/emails`).
    for path in ["/v1/company/features", "/v1/email-attachments/1/preview"] {
        server
            .get(path)
            .add_header(HEADER, token(301))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
    // The app's own routes took the token everywhere before, and still do.
    server
        .get("/v1/me/devices")
        .add_header(HEADER, token(301))
        .await
        .assert_status_ok();
}

#[test_db::test(seed)]
async fn a_paused_companys_device_token_gets_402(pool: MySqlPool) {
    add_session(&pool, 304, 100, 100).await;
    let (server, _) = phone_server(pool.clone()).await;
    server
        .get(UNREAD)
        .add_header(HEADER, token(304))
        .await
        .assert_status_ok();

    sql(
        &pool,
        "UPDATE company SET billing_required = 1 WHERE id = 100",
    )
    .await;
    sql(
        &pool,
        "INSERT INTO company_billing (company_id, stripe_customer_id, subscription_status, payment_status)
         VALUES (100, 'cus_100', 'canceled', 'suspended')",
    )
    .await;
    for path in [UNREAD, EVENTS] {
        server
            .get(path)
            .add_header(HEADER, token(304))
            .await
            .assert_status(StatusCode::PAYMENT_REQUIRED);
    }
}
