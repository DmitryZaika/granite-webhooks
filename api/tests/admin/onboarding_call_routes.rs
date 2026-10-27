//! `/v1/onboarding-call` and `/v1/onboarding-call/bookings` without a fake
//! Calendly: who may book, and why booking is unavailable. Seed company
//! 20400 has a pending commitment (Owen Manager 20400); the booking flow
//! itself is tested in `onboarding_call.rs`.

use crate::common;

use api::app;
use api::auth::cookie::SessionData;
use api::onboarding_call::Config;
use api::onboarding_call::calendly::Calendly;
use api::state::AppState;
use axum::http::StatusCode;
use axum_test::TestServer;
use common::{ALLOWED_ORIGIN, SECRET, WithSession, cookie_for, server};
use serde_json::{Value, json};
use sqlx::MySqlPool;

const OWEN: &str = "aaaaaaaa-0000-4000-8000-000000020400";
const PENDING: i32 = 20400;

async fn status(pool: &MySqlPool) -> String {
    sqlx::query_scalar("SELECT status FROM company_onboarding_commitments WHERE company_id = ?")
        .bind(PENDING)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[test_db::test(seed)]
async fn a_superuser_acting_in_the_company_cannot_book(pool: MySqlPool) {
    // Sam Super (103) becomes a Granite superuser with a super admin position
    // in 20400, so acts there as an admin.
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 103")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users_positions (user_id, position_id, company_id) VALUES (103, 9, ?)",
    )
    .bind(PENDING)
    .execute(&pool)
    .await
    .unwrap();
    let acting = cookie_for(SessionData {
        session_id: Some(common::sessions::SUPER_ADMIN.to_string()),
        active_company_id: Some(PENDING),
    });
    let server = server(pool.clone());

    let me: Value = server
        .get("/v1/me")
        .add_header("cookie", acting.clone())
        .await
        .json();
    assert_eq!(me["company_id"], PENDING);
    assert_eq!(me["is_admin"], true);

    let body: Value = server
        .get("/v1/onboarding-call")
        .add_header("cookie", acting.clone())
        .await
        .json();
    assert_eq!(body["commitment"]["status"], "pending");
    assert_eq!(body["can_book"], false);
    assert_eq!(body["booking_url"], Value::Null);
    assert_eq!(body["booking_unavailable"], false);
    assert_eq!(body["booking_unavailable_reason"], Value::Null);

    server
        .post("/v1/onboarding-call/bookings")
        .add_header("cookie", acting)
        .json(&json!({
            "invitee_uri": "https://api.calendly.com/scheduled_events/EVT-1/invitees/INV-1"
        }))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert_eq!(status(&pool).await, "pending");
}

#[test_db::test(seed)]
async fn booking_unavailable_says_whether_it_is_not_set_up_or_temporary(pool: MySqlPool) {
    // No Calendly at all: not set up.
    let body: Value = server(pool.clone())
        .get("/v1/onboarding-call")
        .with_session(OWEN)
        .await
        .json();
    assert_eq!(body["can_book"], true);
    assert_eq!(body["booking_url"], Value::Null);
    assert_eq!(body["booking_unavailable"], true);
    assert_eq!(body["booking_unavailable_reason"], "not_configured");

    // A token but Calendly unreachable: temporary.
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN).with_onboarding_call(Config {
        calendly: Calendly::new(Some("fake-token"), "http://127.0.0.1:1", None),
        fee_charging: false,
    });
    let body: Value = TestServer::new(app(state))
        .get("/v1/onboarding-call")
        .with_session(OWEN)
        .await
        .json();
    assert_eq!(body["can_book"], true);
    assert_eq!(body["booking_url"], Value::Null);
    assert_eq!(body["booking_unavailable"], true);
    assert_eq!(body["booking_unavailable_reason"], "temporary");
}
