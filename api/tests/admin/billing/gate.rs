//! Ports of `billingGate.test.ts` / `billingAccess.server.test.ts`: the gate, the first-payment window and the 402.

use super::*;
use api::auth::cookie::SessionData;
use chrono::NaiveDateTime;

/// Company 100 has billing switched on and its payment suspended.
async fn suspended_without_stripe(pool: &MySqlPool) {
    sqlx::raw_sql(
        "UPDATE company SET billing_required = 1 WHERE id = 100;
         INSERT INTO company_billing (company_id, stripe_customer_id, subscription_status, payment_status)
         VALUES (100, 'cus_100', 'canceled', 'suspended');",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Company 100 was asked to pay `days_ago` days ago and never subscribed.
async fn asked_to_pay(pool: &MySqlPool, days_ago: i64) {
    sqlx::query(
        "UPDATE company SET billing_required = 1,
                billing_required_at = UTC_TIMESTAMP() - INTERVAL ? DAY
          WHERE id = 100",
    )
    .bind(days_ago)
    .execute(pool)
    .await
    .unwrap();
}

async fn required_at(pool: &MySqlPool) -> Option<NaiveDateTime> {
    sqlx::query_scalar("SELECT billing_required_at FROM company WHERE id = 100")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_payment_required(response: &axum_test::TestResponse) {
    response.assert_status(StatusCode::PAYMENT_REQUIRED);
    response.assert_json(&json!({ "error": "Payment required", "code": "payment_required" }));
}

#[test_db::test(seed)]
async fn a_paused_company_reaches_only_billing_sign_in_and_the_shell(pool: MySqlPool) {
    suspended_without_stripe(&pool).await;
    let server = server(&pool, None).await;
    assert_payment_required(
        &server
            .get("/v1/customers")
            .with_session(sessions::REP)
            .await,
    );
    assert_payment_required(
        &server
            .put("/v1/me/sales-goal")
            .with_session(sessions::REP)
            .json(&json!({ "amount": 1000 }))
            .await,
    );
    for path in [
        "/v1/billing/gate",
        "/v1/me",
        "/v1/app/shell",
        "/v1/notifications",
        "/v1/auth/session",
    ] {
        server
            .get(path)
            .with_session(sessions::REP)
            .await
            .assert_status(StatusCode::OK);
    }
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(gate["restricted"], true);
    assert_eq!(gate["reason"], "payment_failed");
    assert_eq!(gate["pauses_at"], Value::Null);
    // Another company is not affected.
    server
        .get("/v1/customers")
        .with_session(sessions::OTHER_COMPANY_REP)
        .await
        .assert_status(StatusCode::OK);
}

#[test_db::test(seed)]
async fn switched_off_billing_never_pauses(pool: MySqlPool) {
    suspended_without_stripe(&pool).await;
    sqlx::query("UPDATE company SET billing_required = 0 WHERE id = 100")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool, None).await;
    server
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::OK);
}

#[test_db::test(seed)]
async fn a_company_past_its_first_payment_window_is_paused(pool: MySqlPool) {
    asked_to_pay(&pool, 8).await;
    let server = server(&pool, None).await;
    assert_payment_required(
        &server
            .get("/v1/customers")
            .with_session(sessions::REP)
            .await,
    );
    let gate: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(
        gate,
        json!({ "show_warning": false, "days_remaining": 0, "restricted": true,
                "reason": "subscription_required", "pauses_at": null, "can_manage": false })
    );
}

#[test_db::test(seed)]
async fn during_the_first_payment_window_managers_see_when_access_pauses(pool: MySqlPool) {
    manager_and_billing(&pool).await;
    asked_to_pay(&pool, 6).await;
    let server = server(&pool, None).await;
    server
        .get("/v1/customers")
        .with_session(sessions::REP)
        .await
        .assert_status(StatusCode::OK);
    let manager: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::MANAGER)
        .await
        .json();
    assert_eq!(manager["show_warning"], true);
    assert_eq!(manager["days_remaining"], 1);
    assert_eq!(manager["restricted"], false);
    assert_eq!(manager["reason"], "subscription_required");
    let pauses_at = manager["pauses_at"].as_str().unwrap();
    let expected = required_at(&pool).await.unwrap() + chrono::Duration::days(7);
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(pauses_at)
            .unwrap()
            .naive_utc(),
        expected
    );
    let rep: Value = server
        .get("/v1/billing/gate")
        .with_session(sessions::REP)
        .await
        .json();
    assert_eq!(rep["show_warning"], false);
}

#[test_db::test(seed)]
async fn a_super_admin_acting_in_a_paused_company_is_paused_there(pool: MySqlPool) {
    suspended_without_stripe(&pool).await;
    let server = server(&pool, None).await;
    // At home (company 101) nothing is paused.
    server
        .get("/v1/customers")
        .with_session(sessions::SUPER_ADMIN)
        .await
        .assert_status(StatusCode::OK);
    let switched = common::cookie_for(SessionData {
        session_id: Some(sessions::SUPER_ADMIN.into()),
        active_company_id: Some(100),
    });
    assert_payment_required(
        &server
            .get("/v1/customers")
            .add_header("cookie", switched.clone())
            .await,
    );
    server
        .get("/v1/me")
        .add_header("cookie", switched)
        .await
        .assert_status(StatusCode::OK);
}

#[test_db::test(seed)]
async fn turning_payment_on_starts_the_window_once(pool: MySqlPool) {
    sqlx::query("UPDATE users SET is_superuser = 1 WHERE id = 102")
        .execute(&pool)
        .await
        .unwrap();
    let server = server(&pool, None).await;
    let set = |enabled: bool| {
        server
            .put("/v1/superuser/companies/100/billing-required")
            .with_session(sessions::ADMIN)
            .json(&json!({ "enabled": enabled }))
    };
    assert_eq!(required_at(&pool).await, None);
    set(false).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(required_at(&pool).await, None);
    set(true).await.assert_status(StatusCode::NO_CONTENT);
    let started = required_at(&pool).await.expect("window started");
    assert!((api::billing::now() - started).num_seconds().abs() < 60);

    let earlier =
        NaiveDateTime::parse_from_str("2026-01-01 00:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
    sqlx::query("UPDATE company SET billing_required_at = ? WHERE id = 100")
        .bind(earlier)
        .execute(&pool)
        .await
        .unwrap();
    set(true).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(required_at(&pool).await, Some(earlier));
    set(false).await.assert_status(StatusCode::NO_CONTENT);
    assert_eq!(required_at(&pool).await, Some(earlier));
    set(true).await.assert_status(StatusCode::NO_CONTENT);
    assert!(required_at(&pool).await.unwrap() > earlier);
}
