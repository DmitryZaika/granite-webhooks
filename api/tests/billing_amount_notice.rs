//! The amount-change notice of `POST /v1/jobs/billing-reminders`, driven
//! through `process_amount_notices` with an injected "now", against a fake
//! mail sink. Ids 20560-20579 are this file's.

mod common;
#[path = "support/fake_webhooks.rs"]
mod fake_webhooks;

use api::billing::amount_notice::process_amount_notices;
use api::state::AppState;
use chrono::{NaiveDate, NaiveDateTime};
use common::{ALLOWED_ORIGIN, SECRET};
use fake_webhooks::Sink;
use serde_json::Value;
use sqlx::MySqlPool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COMPANY: i32 = 20560;
const LEGACY_COMPANY: i32 = 20570;
const MANAGER_ONE: &str = "ann@amount.test";
const MANAGER_TWO: &str = "ben@amount.test";
const REP: &str = "rep@amount.test";
const LEGACY_MANAGER: &str = "boss@legacy-amount.test";

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

struct Fixture {
    pool: MySqlPool,
    state: AppState,
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

/// A self-serve company on an active subscription at $300.00 a month, the
/// period ending November 21, 2026, and a legacy company Stripe also bills.
async fn fixture(pool: MySqlPool) -> Fixture {
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
    user(&pool, 20560, MANAGER_ONE, COMPANY, 5).await;
    user(&pool, 20561, MANAGER_TWO, COMPANY, 5).await;
    user(&pool, 20562, REP, COMPANY, 1).await;
    user(&pool, 20570, LEGACY_MANAGER, LEGACY_COMPANY, 5).await;
    let sink: fake_webhooks::Shared = Arc::new(Mutex::new(Sink::default()));
    let state = AppState::new(pool.clone(), SECRET, ALLOWED_ORIGIN)
        .with_webhooks(fake_webhooks::start(&sink).await);
    Fixture { pool, state, sink }
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
async fn every_send_failing_gives_the_claim_back(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;
    f.sink.lock().unwrap().fail_emails = 2;

    assert_eq!(f.tick(day_one()).await, 0);

    assert_eq!(f.notice(COMPANY).await, (Some(30_000), None));
    assert_eq!(f.ours(), 0);
    // The next run retries.
    assert_eq!(f.tick(day_one()).await, 1);
    assert_eq!(f.ours(), 2);
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );
}

#[test_db::test(seed)]
async fn one_send_failing_keeps_the_claim(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.set_total(COMPANY, 33_000).await;
    f.sink.lock().unwrap().fail_emails = 1;

    assert_eq!(f.tick(day_one()).await, 1);

    assert_eq!(f.ours(), 1);
    assert_eq!(
        f.notice(COMPANY).await,
        (Some(33_000), Some(day("2026-10-24")))
    );
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
async fn a_subscription_ending_first_moves_the_baseline_quietly(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    sqlx::query("UPDATE company_billing SET cancel_at = current_period_end WHERE company_id = ?")
        .bind(COMPANY)
        .execute(&f.pool)
        .await
        .unwrap();
    f.set_total(COMPANY, 33_000).await;

    assert_eq!(f.tick(day_one()).await, 0);

    assert_eq!(f.ours(), 0);
    assert_eq!(f.notice(COMPANY).await.0, Some(33_000));
}

#[test_db::test(seed)]
async fn legacy_companies_are_ignored(pool: MySqlPool) {
    let f = fixture(pool).await;
    f.tick(day_one()).await;
    f.tick(day_two()).await;

    assert_eq!(f.notice(LEGACY_COMPANY).await, (Some(20_000), None));
    assert!(f.emails_to(LEGACY_MANAGER).is_empty());
}
