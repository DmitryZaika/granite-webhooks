//! The amount-change notice of `POST /v1/jobs/billing-reminders`, driven
//! through `process_amount_notices` with an injected "now", against a fake
//! mail sink. Ids 20560-20579 are this file's.

mod common;
#[path = "support/fake_stripe.rs"]
mod fake_stripe;
#[path = "support/fake_webhooks.rs"]
mod fake_webhooks;

use api::app;
use api::billing::amount_notice::process_amount_notices;
use api::billing::seats::after_user_change;
use api::billing::stripe::signature_header;
use api::state::AppState;
use axum_test::TestServer;
use chrono::{NaiveDate, NaiveDateTime};
use common::{ALLOWED_ORIGIN, SECRET};
use fake_stripe::{FakeStripe, price};
use fake_webhooks::Sink;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COMPANY: i32 = 20560;
const LEGACY_COMPANY: i32 = 20570;
const MANAGER_ONE: &str = "ann@amount.test";
const MANAGER_TWO: &str = "ben@amount.test";
const REP: &str = "rep@amount.test";
const LEGACY_MANAGER: &str = "boss@legacy-amount.test";
const MANAGER_ONE_ID: i32 = 20560;
const MANAGER_TWO_ID: i32 = 20561;
const SUBSCRIPTION: &str = "sub_amount";
const WEBHOOK_SECRET: &str = "whsec_amount_test";

fn dt(text: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
}

/// 15:00 UTC: the same calendar day in Indianapolis.
fn day_one() -> NaiveDateTime {
    dt("2026-10-24 15:00:00")
}

fn day_two() -> NaiveDateTime {
    dt("2026-10-25 15:00:00")
}

fn day_three() -> NaiveDateTime {
    dt("2026-10-26 15:00:00")
}

struct Fixture {
    pool: MySqlPool,
    state: AppState,
    server: TestServer,
    stripe: fake_stripe::Shared,
    sink: fake_webhooks::Shared,
}

async fn user(pool: &MySqlPool, id: i32, email: &str, company: i32, position: i32) {
    sqlx::query(
        "INSERT INTO users (id, email, password, name, is_employee, company_id)
         VALUES (?, ?, 'x', 'Amount User', 1, ?)",
    )
    .bind(id)
    .bind(email)
    .bind(company)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO users_positions (user_id, position_id, company_id) VALUES (?, ?, ?)")
        .bind(id)
        .bind(position)
        .bind(company)
        .execute(pool)
        .await
        .unwrap();
}

/// A self-serve company on an active subscription at $300.00 a month (the
/// base item on the fake Stripe), the period ending November 21, 2026, and a
/// legacy company Stripe also bills.
async fn fixture(pool: MySqlPool) -> Fixture {
    let stripe = FakeStripe::with_catalog();
    stripe.lock().unwrap().subscriptions.insert(
        SUBSCRIPTION.into(),
        json!({
            "id": SUBSCRIPTION, "object": "subscription", "customer": "cus_amount",
            "status": "active", "metadata": { "company_id": COMPANY.to_string() },
            "items": { "object": "list", "data": [{
                "id": "si_amount_base", "object": "subscription_item", "quantity": 1,
                "price": price("price_base", "granite_base_monthly_v1", 30_000),
                "current_period_start": 1_798_000_000, "current_period_end": 1_800_600_000,
            }] },
        }),
    );
    sqlx::query(
        "INSERT INTO company (id, name, plan_source)
         VALUES (?, 'Amount Stone', 'subscription'), (?, 'Legacy Amount Stone', 'legacy')",
    )
    .bind(COMPANY)
    .bind(LEGACY_COMPANY)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO company_billing
           (company_id, stripe_customer_id, stripe_subscription_id, subscription_status,
            payment_status, current_period_end, monthly_price_cents, notified_monthly_cents)
         VALUES (?, 'cus_amount', 'sub_amount', 'active', 'active', '2026-11-21 05:00:00', 30000, NULL),
                (?, 'cus_legacy', 'sub_legacy', 'active', 'active', '2026-11-21 05:00:00', 30000, 20000)",
    )
    .bind(COMPANY)
    .bind(LEGACY_COMPANY)
    .execute(&pool)
    .await
    .unwrap();
    user(&pool, MANAGER_ONE_ID, MANAGER_ONE, COMPANY, 5).await;
    user(&pool, MANAGER_TWO_ID, MANAGER_TWO, COMPANY, 5).await;
    user(&pool, 20562, REP, COMPANY, 1).await;
    user(&pool, 20570, LEGACY_MANAGER, LEGACY_COMPANY, 5).await;
    let sink: fake_webhooks::Shared = Arc::new(Mutex::new(Sink::default()));
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_stripe(fake_stripe::client(&stripe).await)
        .with_webhooks(fake_webhooks::start(&sink).await)
        .with_stripe_webhook_secret(WEBHOOK_SECRET);
    Fixture {
        pool,
        server: TestServer::new(app(state.clone())),
        state,
        stripe,
        sink,
    }
}

impl Fixture {
    async fn tick(&self, now: NaiveDateTime) -> usize {
        process_amount_notices(&self.state, now, Instant::now() + Duration::from_secs(60)).await
    }

    async fn set_total(&self, company: i32, cents: i64) {
        sqlx::query("UPDATE company_billing SET monthly_price_cents = ? WHERE company_id = ?")
            .bind(cents)
            .bind(company)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// `(notified_monthly_cents, amount_notice_on)`.
    async fn notice(&self, company: i32) -> (Option<i64>, Option<NaiveDate>) {
        sqlx::query_as(
            "SELECT CAST(notified_monthly_cents AS SIGNED), amount_notice_on
               FROM company_billing WHERE company_id = ?",
        )
        .bind(company)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// The company's cancellation is scheduled for the end of the period
    /// (`Some`) or withdrawn (`None`), as the cancellation routes store it.
    async fn set_cancel_at(&self, cancel_at: Option<&str>) {
        sqlx::query("UPDATE company_billing SET cancel_at = ? WHERE company_id = ?")
            .bind(cancel_at.map(dt))
            .bind(COMPANY)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    /// `(user_id, status, attempts, step)` of the company's notice sends.
    async fn sends(&self) -> Vec<(i32, String, i32, String)> {
        sqlx::query_as(
            "SELECT user_id, status, attempts, step FROM billing_reminder_emails
              WHERE company_id = ? AND cycle_kind = 'amount_change' ORDER BY user_id, id",
        )
        .bind(COMPANY)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Delivers the fake subscription as a Stripe webhook event.
    async fn post_event(&self, id: &str, kind: &str) {
        let object = self.stripe.lock().unwrap().subscriptions[SUBSCRIPTION].clone();
        let payload = serde_json::to_vec(&json!({
            "id": id, "type": kind, "data": { "object": object },
        }))
        .unwrap();
        let header = signature_header(&payload, WEBHOOK_SECRET, chrono::Utc::now().timestamp());
        self.server
            .post("/v1/webhooks/stripe")
            .add_header("stripe-signature", header)
            .add_header("content-type", "application/json")
            .bytes(payload.into())
            .await
            .assert_status_ok();
    }

    fn emails_to(&self, to: &str) -> Vec<Value> {
        self.sink.lock().unwrap().emails_to(to)
    }

    fn ours(&self) -> usize {
        [MANAGER_ONE, MANAGER_TWO, REP, LEGACY_MANAGER]
            .iter()
            .map(|to| self.emails_to(to).len())
            .sum()
    }
}

fn day(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
}

fn text(email: &Value) -> &str {
    email["text"].as_str().unwrap()
}

fn send(user_id: i32, status: &str, attempts: i32, step: &str) -> (i32, String, i32, String) {
    (user_id, status.to_string(), attempts, step.to_string())
}

const FROM_300_TO_330: &str =
    "changed from $300.00 plus applicable sales tax to $330.00 plus applicable sales tax.";

#[test_db::test(seed)]
async fn the_first_sight_sets_the_baseline_without_email(pool: MySqlPool) {
    let f = fixture(pool).await;

    assert_eq!(f.tick(day_one()).await, 0);

    assert_eq!(f.notice(COMPANY).await, (Some(30_000), None));
    assert_eq!(f.ours(), 0);
    // Nothing changed: still nothing.
    assert_eq!(f.tick(day_two()).await, 0);
    assert_eq!(f.ours(), 0);
}

#[test_db::test(seed)]
async fn a_change_is_sent_once_a_day_with_the_latest_total(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;

    assert_eq!(f.tick(day_one()).await, 1);

    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert_eq!(
            emails[0]["subject"],
            "Your Granite Manager monthly total is now $330.00"
        );
        assert_eq!(emails[0]["transactional"], true);
        let body = text(&emails[0]);
        assert!(body.contains(
            "changed from $300.00 plus applicable sales tax to $330.00 plus applicable sales tax."
        ));
        assert!(body.contains("The new total first applies to the charge on November 21, 2026."));
        assert!(body.contains("prorated adjustment"));
        assert!(body.contains("Each active user above 10 is $30.00 per month."));
    }
    assert!(f.emails_to(REP).is_empty());
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );

    // The same day: no second email, not even for a newer total.
    assert_eq!(f.tick(day_one()).await, 0);
    f.set_total(COMPANY, 36_000).await;
    assert_eq!(f.tick(day_one()).await, 0);
    assert_eq!(f.ours(), 2);

    // The next day: the latest total, from the one last told.
    assert_eq!(f.tick(day_two()).await, 1);
    assert_eq!(f.ours(), 4);
    let latest = f.emails_to(MANAGER_ONE);
    assert!(text(&latest[1]).contains(
        "changed from $330.00 plus applicable sales tax to $360.00 plus applicable sales tax."
    ));
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(36_000), Some(day("2026-10-25")))
    );
}

#[test_db::test(seed)]
async fn every_send_failing_waits_for_the_next_day(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;
    f.sink.lock().unwrap().fail_emails = 2;

    assert_eq!(f.tick(day_one()).await, 0);

    assert_eq!(f.ours(), 0);
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );
    assert_eq!(
        f.sends().await,
        vec![
            send(MANAGER_ONE_ID, "failed", 1, "from_30000"),
            send(MANAGER_TWO_ID, "failed", 1, "from_30000"),
        ]
    );
    // The mail service is back, but nothing is resent the same day.
    assert_eq!(f.tick(day_one()).await, 0);
    assert_eq!(f.ours(), 0);

    // The next day: both, from the total they last heard.
    assert_eq!(f.tick(day_two()).await, 1);
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert!(text(&emails[0]).contains(FROM_300_TO_330), "{manager}");
    }
    assert!(f.emails_to(REP).is_empty());
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-25")))
    );
    assert_eq!(
        f.sends().await,
        vec![
            send(MANAGER_ONE_ID, "sent", 2, "from_30000"),
            send(MANAGER_TWO_ID, "sent", 2, "from_30000"),
        ]
    );
}

#[test_db::test(seed)]
async fn one_failed_manager_gets_it_the_next_day_and_the_other_not_twice(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;
    // Managers are emailed in id order: the first send fails.
    f.sink.lock().unwrap().fail_emails = 1;

    assert_eq!(f.tick(day_one()).await, 1);

    assert!(f.emails_to(MANAGER_ONE).is_empty());
    assert_eq!(f.emails_to(MANAGER_TWO).len(), 1);
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );
    assert_eq!(f.tick(day_one()).await, 0);
    assert_eq!(f.ours(), 1);

    assert_eq!(f.tick(day_two()).await, 1);

    let retried = f.emails_to(MANAGER_ONE);
    assert_eq!(retried.len(), 1);
    assert!(text(&retried[0]).contains(FROM_300_TO_330));
    assert_eq!(f.emails_to(MANAGER_TWO).len(), 1);
    assert_eq!(
        f.sends().await,
        vec![
            send(MANAGER_ONE_ID, "sent", 2, "from_30000"),
            send(MANAGER_TWO_ID, "sent", 1, "from_30000"),
        ]
    );
    // Nothing is left to send.
    assert_eq!(f.tick(day_three()).await, 0);
    assert_eq!(f.ours(), 2);
}

#[test_db::test(seed)]
async fn a_failed_manager_hears_the_latest_total_from_the_one_they_knew(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;
    f.sink.lock().unwrap().fail_emails = 1;
    assert_eq!(f.tick(day_one()).await, 1);
    f.set_total(COMPANY, 36_000).await;

    assert_eq!(f.tick(day_two()).await, 1);

    let first = f.emails_to(MANAGER_ONE);
    assert_eq!(first.len(), 1);
    assert!(text(&first[0]).contains(
        "changed from $300.00 plus applicable sales tax to $360.00 plus applicable sales tax."
    ));
    let second = f.emails_to(MANAGER_TWO);
    assert_eq!(second.len(), 2);
    assert!(text(&second[1]).contains(
        "changed from $330.00 plus applicable sales tax to $360.00 plus applicable sales tax."
    ));
    assert_eq!(
        f.sends().await,
        vec![
            send(MANAGER_ONE_ID, "skipped", 1, "from_30000"),
            send(MANAGER_ONE_ID, "sent", 1, "from_30000"),
            send(MANAGER_TWO_ID, "sent", 1, "from_30000"),
            send(MANAGER_TWO_ID, "sent", 1, "from_33000"),
        ]
    );
    assert_eq!(f.tick(day_three()).await, 0);
    assert_eq!(f.ours(), 3);
}

#[test_db::test(seed)]
async fn a_free_period_change_names_the_first_charge_without_proration(pool: MySqlPool) {
    let f = fixture(pool).await;
    sqlx::query(
        "UPDATE company_billing SET subscription_status = 'trialing',
                trial_end_at = '2027-01-21 05:00:00'
          WHERE company_id = ?",
    )
    .bind(COMPANY)
    .execute(&f.pool)
    .await
    .unwrap();
    f.tick(day_one()).await;
    f.set_total(COMPANY, 37_500).await;

    assert_eq!(f.tick(day_one()).await, 1);

    let body = text(&f.emails_to(MANAGER_ONE)[0]).to_string();
    assert!(body.contains("The new total first applies to the charge on January 21, 2027."));
    assert!(!body.contains("prorated"));
}

#[test_db::test(seed)]
async fn a_change_while_cancelling_waits_quietly(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_cancel_at(Some("2026-11-21 05:00:00")).await;
    f.set_total(COMPANY, 33_000).await;

    assert_eq!(f.tick(day_one()).await, 0);
    assert_eq!(f.tick(day_two()).await, 0);

    assert_eq!(f.ours(), 0);
    // The total the Managers know is kept, so a withdrawal announces it.
    assert_eq!(f.notice(COMPANY).await, (Some(30_000), None));
    assert!(f.sends().await.is_empty());
}

#[test_db::test(seed)]
async fn withdrawing_then_ticking_announces_the_change(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_cancel_at(Some("2026-11-21 05:00:00")).await;
    f.set_total(COMPANY, 33_000).await;
    assert_eq!(f.tick(day_one()).await, 0);

    f.set_cancel_at(None).await;

    assert_eq!(f.tick(day_one()).await, 1);
    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let emails = f.emails_to(manager);
        assert_eq!(emails.len(), 1, "{manager}");
        assert!(text(&emails[0]).contains(FROM_300_TO_330), "{manager}");
    }
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );
}

#[test_db::test(seed)]
async fn a_seat_push_announces_the_new_total(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    // Two Managers and a rep, plus 8 reps: 11 users, one above the 10 included.
    for id in 20571..=20578 {
        user(&f.pool, id, &format!("rep{id}@amount.test"), COMPANY, 1).await;
    }

    after_user_change(&f.state, COMPANY).await;

    let extra: Vec<i64> = f.stripe.lock().unwrap().subscriptions[SUBSCRIPTION]["items"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["price"]["lookup_key"] == "granite_extra_user_monthly_v1")
        .map(|item| item["quantity"].as_i64().unwrap())
        .collect();
    assert_eq!(extra, vec![1]);
    f.post_event("evt_amount_seats", "customer.subscription.updated")
        .await;
    let monthly: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(monthly_price_cents AS SIGNED) FROM company_billing WHERE company_id = ?",
    )
    .bind(COMPANY)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(monthly, Some(33_000));

    assert_eq!(f.tick(day_one()).await, 1);

    for manager in [MANAGER_ONE, MANAGER_TWO] {
        let notices: Vec<Value> = f
            .emails_to(manager)
            .into_iter()
            .filter(|email| email["subject"] == "Your Granite Manager monthly total is now $330.00")
            .collect();
        assert_eq!(notices.len(), 1, "{manager}");
        assert!(text(&notices[0]).contains(FROM_300_TO_330), "{manager}");
    }
    assert!(f.emails_to(REP).is_empty());
}

#[test_db::test(seed)]
async fn legacy_companies_are_ignored(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.tick(day_two()).await;

    assert_eq!(f.notice(LEGACY_COMPANY).await, (Some(20_000), None));
    assert!(f.emails_to(LEGACY_MANAGER).is_empty());
}
