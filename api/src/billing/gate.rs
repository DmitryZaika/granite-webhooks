//! The access gate: whether a company's access is paused, and its warning.
//!
//! The warning covers a failed payment's grace period, or the first-payment
//! window of a company that never subscribed. Stripe is asked (throttled)
//! before a company is paused or while it is.
// Generic over the store, like `machine`.
#![allow(clippy::future_not_send)]

use super::lock::CompanyLock;
use super::machine::restore_active;
use super::reactivation::{current_subscription, refresh_subscription};
use super::store::{BillingStore, MySqlBillingStore, StoreResult};
use super::stripe::{Stripe, StripeError};
use super::{
    CompanyBilling, PaymentStatus, StripeLiveState, days_remaining, first_payment_deadline,
    is_awaiting_first_payment, is_overdue, stripe_confirms_non_payment, with_grace_expiry,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};

const PAST_DUE_REFRESH_SECS: i64 = 15 * 60;
const SUSPENDED_REFRESH_SECS: i64 = 60;
const LIVE_CHECK_RETRY_SECS: i64 = 5 * 60;

/// When each company was last refreshed from / checked with Stripe (per
/// Lambda container, as the CRM kept it per server process), by Stripe base
/// and company id.
type Checks = Mutex<HashMap<(String, i32), NaiveDateTime>>;
static LAST_REFRESH: LazyLock<Checks> = LazyLock::new(Mutex::default);
static LAST_LIVE_CHECK: LazyLock<Checks> = LazyLock::new(Mutex::default);
/// For gate failures reported to `PostHog`.
static POSTHOG_HTTP: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// Why a billing notice is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BillingNoticeReason {
    /// A subscription payment failed, or the subscription ended unpaid.
    PaymentFailed,
    /// Stripe payment was turned on for the company and it has not
    /// subscribed yet.
    SubscriptionRequired,
}

/// What a page shows about billing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct BillingNotice {
    /// Show the billing warning banner (managers only, while access still
    /// works).
    pub show_warning: bool,
    /// Whole days left before access pauses (rounded up); 0 when nothing
    /// runs or access is already paused.
    pub days_remaining: i64,
    /// Access is paused: only the billing pages work, other API calls answer
    /// 402.
    pub restricted: bool,
    /// Why the notice is shown; null when billing is in order.
    pub reason: Option<BillingNoticeReason>,
    /// When access will pause (UTC), while a grace period or first-payment
    /// window runs; null otherwise.
    pub pauses_at: Option<DateTime<Utc>>,
}

pub const NO_BILLING_NOTICE: BillingNotice = BillingNotice {
    show_warning: false,
    days_remaining: 0,
    restricted: false,
    reason: None,
    pauses_at: None,
};

/// Stripe and the company lock as the gate sees them. Stripe calls are
/// throttled and swallow errors.
#[allow(async_fn_in_trait)] // used with concrete types only
pub trait GateStripe {
    /// A held company lock.
    type Lock;
    /// A refresh, or the end-of-grace live check, would call Stripe now. The
    /// lock is taken only when it is, so page loads between checks never
    /// queue on it.
    fn check_due(&self, record: &CompanyBilling) -> bool;
    /// The record after applying Stripe's current subscription.
    async fn refresh(&self, record: CompanyBilling) -> CompanyBilling;
    /// Stripe's word on a company whose grace period ended; `None` when it
    /// could not be asked.
    async fn live_state(&self, record: &CompanyBilling) -> Option<StripeLiveState>;
    /// The company's lock; `None` when it is busy or failed (logged).
    async fn lock(&self, company_id: i32) -> Option<Self::Lock>;
    async fn unlock(&self, lock: Self::Lock);
}

/// Runs `work` under the company lock; without the lock, gives `fallback`
/// (the record read without it).
async fn locked<G: GateStripe, T>(
    stripe: &G,
    company_id: i32,
    fallback: T,
    work: impl Future<Output = StoreResult<T>>,
) -> StoreResult<T> {
    let Some(lock) = stripe.lock(company_id).await else {
        return Ok(fallback);
    };
    let result = work.await;
    stripe.unlock(lock).await;
    result
}

fn grace_ended(record: &CompanyBilling, now: NaiveDateTime) -> bool {
    record.payment_status == PaymentStatus::PastDue
        && with_grace_expiry(record, now).payment_status == PaymentStatus::Suspended
}

/// Refreshes an overdue record from Stripe and applies the end of its grace
/// period. Runs under the company lock.
async fn settle_overdue_record<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    existing: CompanyBilling,
    now: NaiveDateTime,
) -> StoreResult<CompanyBilling> {
    if !is_overdue(&existing) {
        return Ok(existing);
    }
    let mut current = stripe.refresh(existing).await;
    if grace_ended(&current, now)
        && let Some(live) = stripe.live_state(&current).await
    {
        if stripe_confirms_non_payment(&live) {
            current.payment_status = PaymentStatus::Suspended;
            store.save(&current).await?;
        } else {
            current = restore_active(&current, current.last_successful_payment_at, now);
            store.save(&current).await?;
            store
                .clear_payment_notifications(current.company_id)
                .await?;
        }
    }
    Ok(current)
}

/// `settle_overdue_record` on the row re-read inside the lock, so a webhook
/// landing at the same moment is not overwritten.
async fn settle_overdue<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    fallback: &CompanyBilling,
    now: NaiveDateTime,
) -> StoreResult<CompanyBilling> {
    let existing = store
        .by_company(fallback.company_id)
        .await?
        .unwrap_or_else(|| fallback.clone());
    settle_overdue_record(store, stripe, existing, now).await
}

/// Asks Stripe about a company that never paid, when it has a Stripe
/// customer and a check is due. Runs under the company lock.
async fn refresh_first_payment<G: GateStripe>(
    stripe: &G,
    existing: Option<CompanyBilling>,
) -> Option<CompanyBilling> {
    match existing {
        Some(record) if record.stripe_customer_id.is_some() && stripe.check_due(&record) => {
            Some(stripe.refresh(record).await)
        }
        other => other,
    }
}

/// Before pausing a company that never subscribed, ask Stripe once more
/// (when a check is due): a Checkout may have completed while its webhook is
/// still on the way.
async fn recheck_first_payment<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    company_id: i32,
    existing: Option<CompanyBilling>,
) -> StoreResult<Option<CompanyBilling>> {
    let due = existing
        .as_ref()
        .is_some_and(|record| record.stripe_customer_id.is_some() && stripe.check_due(record));
    if !due {
        return Ok(existing);
    }
    let fallback = existing.clone();
    locked(stripe, company_id, fallback, async {
        let current = store.by_company(company_id).await?.or(existing);
        if !is_awaiting_first_payment(current.as_ref()) {
            return Ok(current);
        }
        Ok(refresh_first_payment(stripe, current).await)
    })
    .await
}

async fn manager_sees_warning<S: BillingStore>(
    store: &S,
    company_id: i32,
    user_id: i32,
) -> StoreResult<bool> {
    Ok(store.managers(company_id).await?.contains(&user_id))
}

enum FirstPayment {
    Notice(BillingNotice),
    /// Stripe shows a payment after all: the normal rules apply.
    Paid(Box<CompanyBilling>),
}

/// A company asked to pay that has never subscribed: a warning while the
/// `FIRST_PAYMENT_DAYS` window runs (from `billing_required_at`; unknown
/// counts as now, so it never pauses), then paused until it subscribes.
async fn first_payment_notice<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    company_id: i32,
    user_id: i32,
    existing: Option<CompanyBilling>,
    now: NaiveDateTime,
) -> StoreResult<FirstPayment> {
    let since = store
        .billing_required_since(company_id)
        .await?
        .unwrap_or(now);
    let deadline = first_payment_deadline(since);
    if now < deadline {
        return Ok(FirstPayment::Notice(BillingNotice {
            show_warning: manager_sees_warning(store, company_id, user_id).await?,
            days_remaining: days_remaining(deadline, now),
            restricted: false,
            reason: Some(BillingNoticeReason::SubscriptionRequired),
            pauses_at: Some(deadline.and_utc()),
        }));
    }
    match recheck_first_payment(store, stripe, company_id, existing).await? {
        Some(record) if !is_awaiting_first_payment(Some(&record)) => {
            Ok(FirstPayment::Paid(Box::new(record)))
        }
        _ => Ok(FirstPayment::Notice(BillingNotice {
            restricted: true,
            reason: Some(BillingNoticeReason::SubscriptionRequired),
            ..NO_BILLING_NOTICE
        })),
    }
}

/// The gate's pause decision for one company, outside a page load.
///
/// The reminder emails run it so "access is paused" goes out only when a page
/// load would really be paused: a failed payment once Stripe confirms it, a
/// company that never subscribed once its window ended and Stripe shows no
/// payment. Without the lock (busy), gives the record as stored.
pub async fn settle_company_billing<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    company_id: i32,
    required_since: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> StoreResult<Option<CompanyBilling>> {
    let Some(lock) = stripe.lock(company_id).await else {
        return store.by_company(company_id).await;
    };
    let result = settle_locked(store, stripe, company_id, required_since, now).await;
    stripe.unlock(lock).await;
    result
}

/// `settle_company_billing` for a caller that already holds the company lock.
pub async fn settle_locked<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    company_id: i32,
    required_since: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> StoreResult<Option<CompanyBilling>> {
    match store.by_company(company_id).await? {
        Some(existing) if !is_awaiting_first_payment(Some(&existing)) => {
            settle_overdue_record(store, stripe, existing, now)
                .await
                .map(Some)
        }
        existing => {
            let window_ended =
                required_since.is_some_and(|since| now >= first_payment_deadline(since));
            Ok(if window_ended {
                refresh_first_payment(stripe, existing).await
            } else {
                existing
            })
        }
    }
}

/// Whether the company's access is paused, and the warning for `user_id`.
/// Read failures are errors, never "no billing row" (that would pause a
/// paying company as one that never subscribed).
pub async fn evaluate_gate<S: BillingStore, G: GateStripe>(
    store: &S,
    stripe: &G,
    company_id: i32,
    user_id: i32,
    now: NaiveDateTime,
) -> StoreResult<BillingNotice> {
    if !store.billing_required(company_id).await? {
        return Ok(NO_BILLING_NOTICE);
    }
    let mut existing = store.by_company(company_id).await?;
    if is_awaiting_first_payment(existing.as_ref()) {
        match first_payment_notice(store, stripe, company_id, user_id, existing, now).await? {
            FirstPayment::Notice(notice) => return Ok(notice),
            FirstPayment::Paid(record) => existing = Some(*record),
        }
    }
    let Some(record) = existing else {
        return Ok(NO_BILLING_NOTICE);
    };
    let current = if is_overdue(&record) && stripe.check_due(&record) {
        locked(
            stripe,
            company_id,
            record.clone(),
            settle_overdue(store, stripe, &record, now),
        )
        .await?
    } else {
        record
    };
    match current.payment_status {
        PaymentStatus::Suspended => Ok(BillingNotice {
            restricted: true,
            reason: Some(BillingNoticeReason::PaymentFailed),
            ..NO_BILLING_NOTICE
        }),
        PaymentStatus::PastDue => Ok(BillingNotice {
            show_warning: manager_sees_warning(store, company_id, user_id).await?,
            days_remaining: current
                .grace_period_ends_at
                .map_or(0, |ends| days_remaining(ends, now)),
            restricted: false,
            reason: Some(BillingNoticeReason::PaymentFailed),
            pauses_at: current.grace_period_ends_at.map(|ends| ends.and_utc()),
        }),
        PaymentStatus::Active | PaymentStatus::None => Ok(NO_BILLING_NOTICE),
    }
}

/// Stripe and the MySQL company lock for the gate, throttled per company.
pub struct LiveGate<'a> {
    pub stripe: &'a Stripe,
    pub pool: &'a MySqlPool,
    pub now: NaiveDateTime,
}

/// Paused companies, and ones that never paid, are checked often so a
/// payment in Stripe unlocks them quickly.
const fn refresh_every_secs(record: &CompanyBilling) -> i64 {
    match record.payment_status {
        PaymentStatus::Suspended | PaymentStatus::None => SUSPENDED_REFRESH_SECS,
        PaymentStatus::Active | PaymentStatus::PastDue => PAST_DUE_REFRESH_SECS,
    }
}

fn is_due(map: &Checks, base: &str, company_id: i32, now: NaiveDateTime, every_secs: i64) -> bool {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(base.to_string(), company_id))
        .is_none_or(|at| (now - *at).num_seconds() >= every_secs)
}

/// `is_due`, and if so marks the check as made now.
fn take_due(
    map: &Checks,
    base: &str,
    company_id: i32,
    now: NaiveDateTime,
    every_secs: i64,
) -> bool {
    let mut last = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (base.to_string(), company_id);
    if last
        .get(&key)
        .is_some_and(|at| (now - *at).num_seconds() < every_secs)
    {
        return false;
    }
    last.insert(key, now);
    true
}

/// A refresh, or the end-of-grace live check, is due (Stripe aside).
fn stripe_check_due(base: &str, record: &CompanyBilling, now: NaiveDateTime) -> bool {
    is_due(
        &LAST_REFRESH,
        base,
        record.company_id,
        now,
        refresh_every_secs(record),
    ) || (grace_ended(record, now)
        && is_due(
            &LAST_LIVE_CHECK,
            base,
            record.company_id,
            now,
            LIVE_CHECK_RETRY_SECS,
        ))
}

impl GateStripe for LiveGate<'_> {
    type Lock = CompanyLock;

    fn check_due(&self, record: &CompanyBilling) -> bool {
        self.stripe.is_enabled() && stripe_check_due(self.stripe.base_url(), record, self.now)
    }

    async fn refresh(&self, record: CompanyBilling) -> CompanyBilling {
        if !take_due(
            &LAST_REFRESH,
            self.stripe.base_url(),
            record.company_id,
            self.now,
            refresh_every_secs(&record),
        ) {
            return record;
        }
        let store = MySqlBillingStore(self.pool);
        match refresh_subscription(self.stripe, &store, record.clone(), self.now).await {
            Ok(refreshed) => refreshed,
            Err(err) => {
                tracing::warn!(error = %err, "billing refresh from Stripe failed");
                record
            }
        }
    }

    async fn live_state(&self, record: &CompanyBilling) -> Option<StripeLiveState> {
        if !self.stripe.is_enabled()
            || !take_due(
                &LAST_LIVE_CHECK,
                self.stripe.base_url(),
                record.company_id,
                self.now,
                LIVE_CHECK_RETRY_SECS,
            )
        {
            return None;
        }
        let result = async {
            let subscription = current_subscription(self.stripe, record).await?;
            let open_invoice_cents = match &record.stripe_customer_id {
                Some(customer) => self
                    .stripe
                    .open_invoices(customer)
                    .await?
                    .iter()
                    .filter_map(|invoice| invoice["amount_remaining"].as_i64())
                    .sum(),
                None => 0,
            };
            Ok::<_, StripeError>(StripeLiveState {
                subscription_status: subscription
                    .and_then(|s| s["status"].as_str().map(str::to_string)),
                open_invoice_cents,
            })
        }
        .await;
        result.ok()
    }

    async fn lock(&self, company_id: i32) -> Option<CompanyLock> {
        match CompanyLock::acquire(self.pool, company_id).await {
            Ok(lock) => Some(lock),
            Err(err) => {
                tracing::warn!(error = %err, company_id, "billing gate ran without the company lock");
                None
            }
        }
    }

    async fn unlock(&self, lock: CompanyLock) {
        lock.release().await;
    }
}

/// `settle_locked` on MySQL and Stripe (the reminder emails, which hold the
/// company lock while they settle and send).
pub async fn settle_company_locked(
    stripe: &Stripe,
    pool: &MySqlPool,
    company_id: i32,
    required_since: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> StoreResult<Option<CompanyBilling>> {
    let gate = LiveGate { stripe, pool, now };
    settle_locked(
        &MySqlBillingStore(pool),
        &gate,
        company_id,
        required_since,
        now,
    )
    .await
}

/// The access gate for a page load. Errors keep access (like the CRM) and go
/// to `PostHog`'s Error Tracking.
pub async fn billing_gate(
    stripe: &Stripe,
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> BillingNotice {
    let now = super::now();
    let gate = LiveGate { stripe, pool, now };
    match evaluate_gate(&MySqlBillingStore(pool), &gate, company_id, user_id, now).await {
        Ok(notice) => notice,
        Err(err) => {
            tracing::warn!(error = %err, company_id, "billing gate failed");
            report_gate_error(&err, company_id).await;
            NO_BILLING_NOTICE
        }
    }
}

async fn report_gate_error(err: &sqlx::Error, company_id: i32) {
    let properties = serde_json::json!({
        "$exception_list": [{
            "type": "BillingGateError",
            "value": format!("billing gate failed open: {err}"),
            "mechanism": { "handled": true, "synthetic": false },
        }],
        "$process_person_profile": false,
        "error_source": "api",
        "company_id": company_id,
    });
    crate::posthog::capture(&POSTHOG_HTTP, "$exception", "granite-api", properties).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::{Memory, at};
    use crate::billing::{FIRST_PAYMENT_DAYS, add_grace_days};
    use chrono::Duration;
    use std::cell::Cell;

    struct FakeStripe {
        live: Option<StripeLiveState>,
        refresh: fn(CompanyBilling) -> CompanyBilling,
        due: bool,
        busy: bool,
        locks: Cell<u32>,
        refreshes: Cell<u32>,
    }

    impl Default for FakeStripe {
        fn default() -> Self {
            Self {
                live: None,
                refresh: |record| record,
                due: true,
                busy: false,
                locks: Cell::new(0),
                refreshes: Cell::new(0),
            }
        }
    }

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    impl GateStripe for FakeStripe {
        type Lock = ();
        fn check_due(&self, _: &CompanyBilling) -> bool {
            self.due
        }
        async fn refresh(&self, record: CompanyBilling) -> CompanyBilling {
            self.refreshes.set(self.refreshes.get() + 1);
            (self.refresh)(record)
        }
        async fn live_state(&self, _: &CompanyBilling) -> Option<StripeLiveState> {
            self.live.clone()
        }
        async fn lock(&self, _: i32) -> Option<()> {
            self.locks.set(self.locks.get() + 1);
            (!self.busy).then_some(())
        }
        async fn unlock(&self, (): ()) {}
    }

    fn failed_at() -> NaiveDateTime {
        at("2026-09-01 00:00:00")
    }

    fn past_due() -> CompanyBilling {
        CompanyBilling {
            stripe_customer_id: Some("cus_1".into()),
            stripe_subscription_id: Some("sub_1".into()),
            subscription_status: "past_due".into(),
            payment_status: PaymentStatus::PastDue,
            failed_payment_at: Some(failed_at()),
            grace_period_ends_at: Some(add_grace_days(failed_at())),
            ..CompanyBilling::empty(1)
        }
    }

    fn after_grace() -> NaiveDateTime {
        add_grace_days(failed_at()) + Duration::minutes(1)
    }

    fn live(status: &str, cents: i64) -> FakeStripe {
        FakeStripe {
            live: Some(StripeLiveState {
                subscription_status: Some(status.into()),
                open_invoice_cents: cents,
            }),
            ..FakeStripe::default()
        }
    }

    fn with_record(record: CompanyBilling) -> Memory {
        let store = Memory::default();
        store.records.borrow_mut().insert(record.company_id, record);
        store
    }

    fn paid(record: CompanyBilling) -> CompanyBilling {
        CompanyBilling {
            stripe_subscription_id: Some("sub_1".into()),
            subscription_status: "active".into(),
            payment_status: PaymentStatus::Active,
            ..record
        }
    }

    // ---- billingGate.test.ts: billing access gate

    #[tokio::test]
    async fn switch_off_keeps_a_company_suspended_in_stripe_working() {
        let store = with_record(CompanyBilling {
            payment_status: PaymentStatus::Suspended,
            ..past_due()
        });
        store.not_required.borrow_mut().insert(1);
        let notice = evaluate_gate(&store, &FakeStripe::default(), 1, 10, after_grace())
            .await
            .unwrap();
        assert_eq!(notice, NO_BILLING_NOTICE);
    }

    #[tokio::test]
    async fn grace_ended_but_stripe_shows_paid_restores_access() {
        let store = with_record(past_due());
        let notice = evaluate_gate(&store, &live("active", 0), 1, 10, after_grace())
            .await
            .unwrap();
        assert_eq!(notice, NO_BILLING_NOTICE);
        assert_eq!(store.record(1).payment_status, PaymentStatus::Active);
    }

    #[tokio::test]
    async fn grace_ended_and_stripe_unreachable_keeps_access() {
        let store = with_record(past_due());
        let notice = evaluate_gate(&store, &FakeStripe::default(), 1, 10, after_grace())
            .await
            .unwrap();
        assert!(!notice.restricted);
        assert_eq!(store.record(1).payment_status, PaymentStatus::PastDue);
    }

    #[tokio::test]
    async fn grace_ended_and_unpaid_in_stripe_suspends() {
        let store = with_record(past_due());
        let notice = evaluate_gate(&store, &live("past_due", 4900), 1, 10, after_grace())
            .await
            .unwrap();
        assert_eq!(
            notice,
            BillingNotice {
                restricted: true,
                reason: Some(BillingNoticeReason::PaymentFailed),
                ..NO_BILLING_NOTICE
            }
        );
        assert_eq!(store.record(1).payment_status, PaymentStatus::Suspended);
    }

    #[tokio::test]
    async fn page_loads_between_stripe_checks_do_not_take_the_lock() {
        let store = with_record(past_due());
        let stripe = FakeStripe {
            due: false,
            ..FakeStripe::default()
        };
        let notice = evaluate_gate(&store, &stripe, 1, 10, failed_at() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(stripe.locks.get(), 0);
        assert_eq!(stripe.refreshes.get(), 0);
        assert!(!notice.restricted);
        assert_eq!(notice.reason, Some(BillingNoticeReason::PaymentFailed));
    }

    #[tokio::test]
    async fn a_due_check_settles_under_the_lock() {
        let store = with_record(past_due());
        let stripe = FakeStripe::default();
        evaluate_gate(&store, &stripe, 1, 10, failed_at() + Duration::days(1))
            .await
            .unwrap();
        assert_eq!((stripe.locks.get(), stripe.refreshes.get()), (1, 1));
    }

    #[tokio::test]
    async fn a_busy_lock_falls_back_to_the_stored_record() {
        let store = with_record(past_due());
        let stripe = FakeStripe {
            busy: true,
            ..live("past_due", 4900)
        };
        let notice = evaluate_gate(&store, &stripe, 1, 10, after_grace())
            .await
            .unwrap();
        assert_eq!(stripe.refreshes.get(), 0);
        assert!(!notice.restricted);
        assert_eq!(store.record(1).payment_status, PaymentStatus::PastDue);
    }

    #[tokio::test]
    async fn a_suspended_company_that_paid_is_unlocked() {
        let store = with_record(CompanyBilling {
            payment_status: PaymentStatus::Suspended,
            ..past_due()
        });
        let stripe = FakeStripe {
            refresh: paid,
            ..FakeStripe::default()
        };
        let notice = evaluate_gate(&store, &stripe, 1, 10, after_grace())
            .await
            .unwrap();
        assert!(!notice.restricted);
    }

    #[tokio::test]
    async fn during_grace_only_managers_see_the_warning() {
        let store = with_record(past_due());
        store
            .users
            .borrow_mut()
            .extend([(10, 1, true), (12, 1, false)]);
        let during = at("2026-09-03 00:00:00");
        let manager = evaluate_gate(&store, &FakeStripe::default(), 1, 10, during)
            .await
            .unwrap();
        let admin = evaluate_gate(&store, &FakeStripe::default(), 1, 12, during)
            .await
            .unwrap();
        assert_eq!(
            manager,
            BillingNotice {
                show_warning: true,
                days_remaining: 5,
                restricted: false,
                reason: Some(BillingNoticeReason::PaymentFailed),
                pauses_at: Some(at("2026-09-08 00:00:00").and_utc()),
            }
        );
        assert_eq!(
            serde_json::to_value(manager).unwrap()["pauses_at"],
            "2026-09-08T00:00:00Z"
        );
        assert!(!admin.show_warning);
    }

    // ---- billingGate.test.ts: a company asked to pay that never subscribed

    fn required_since() -> NaiveDateTime {
        at("2026-10-01 00:00:00")
    }

    fn after_window() -> NaiveDateTime {
        first_payment_deadline(required_since()) + Duration::seconds(1)
    }

    fn asked_to_pay(record: Option<CompanyBilling>) -> Memory {
        let store = Memory::default();
        store
            .required_since
            .borrow_mut()
            .insert(1, required_since());
        if let Some(record) = record {
            store.records.borrow_mut().insert(1, record);
        }
        store
    }

    fn checkout_never_completed() -> CompanyBilling {
        CompanyBilling {
            stripe_customer_id: Some("cus_1".into()),
            ..CompanyBilling::empty(1)
        }
    }

    #[tokio::test]
    async fn has_seven_days_and_the_manager_sees_how_many_are_left() {
        let store = asked_to_pay(None);
        store
            .users
            .borrow_mut()
            .extend([(10, 1, true), (11, 1, false)]);
        let day_two = required_since() + Duration::days(2);
        let manager = evaluate_gate(&store, &FakeStripe::default(), 1, 10, day_two)
            .await
            .unwrap();
        let sales_rep = evaluate_gate(&store, &FakeStripe::default(), 1, 11, day_two)
            .await
            .unwrap();
        assert_eq!(FIRST_PAYMENT_DAYS, 7);
        assert_eq!(
            manager,
            BillingNotice {
                show_warning: true,
                days_remaining: 5,
                restricted: false,
                reason: Some(BillingNoticeReason::SubscriptionRequired),
                pauses_at: Some(at("2026-10-08 00:00:00").and_utc()),
            }
        );
        assert!(!sales_rep.show_warning && !sales_rep.restricted);
    }

    #[tokio::test]
    async fn is_paused_once_the_seven_days_end() {
        let store = asked_to_pay(None);
        let stripe = FakeStripe::default();
        let notice = evaluate_gate(&store, &stripe, 1, 10, after_window())
            .await
            .unwrap();
        assert_eq!(
            notice,
            BillingNotice {
                restricted: true,
                reason: Some(BillingNoticeReason::SubscriptionRequired),
                ..NO_BILLING_NOTICE
            }
        );
        // No Stripe customer: nothing to ask Stripe about.
        assert_eq!((stripe.locks.get(), stripe.refreshes.get()), (0, 0));
    }

    #[tokio::test]
    async fn a_stripe_customer_whose_checkout_never_completed_is_paused_too() {
        let store = asked_to_pay(Some(checkout_never_completed()));
        let stripe = FakeStripe::default();
        let notice = evaluate_gate(&store, &stripe, 1, 10, after_window())
            .await
            .unwrap();
        assert!(notice.restricted);
        assert_eq!((stripe.locks.get(), stripe.refreshes.get()), (1, 1));
    }

    #[tokio::test]
    async fn is_not_paused_when_stripe_shows_the_subscription_was_paid() {
        let store = asked_to_pay(Some(checkout_never_completed()));
        let stripe = FakeStripe {
            refresh: paid,
            ..FakeStripe::default()
        };
        let notice = evaluate_gate(&store, &stripe, 1, 10, after_window())
            .await
            .unwrap();
        assert_eq!(notice, NO_BILLING_NOTICE);
    }

    #[tokio::test]
    async fn a_subscribed_company_is_never_held_to_the_window() {
        let store = asked_to_pay(Some(paid(checkout_never_completed())));
        let notice = evaluate_gate(
            &store,
            &FakeStripe::default(),
            1,
            10,
            after_window() + Duration::days(30),
        )
        .await
        .unwrap();
        assert_eq!(notice, NO_BILLING_NOTICE);
    }

    #[tokio::test]
    async fn with_stripe_payment_off_nothing_changes() {
        let store = asked_to_pay(None);
        store.not_required.borrow_mut().insert(1);
        let notice = evaluate_gate(&store, &FakeStripe::default(), 1, 10, after_window())
            .await
            .unwrap();
        assert_eq!(notice, NO_BILLING_NOTICE);
    }

    #[tokio::test]
    async fn an_unknown_start_date_warns_but_never_pauses() {
        let store = Memory::default();
        let now = after_grace();
        let notice = evaluate_gate(&store, &FakeStripe::default(), 1, 10, now)
            .await
            .unwrap();
        assert!(!notice.restricted);
        assert_eq!(notice.days_remaining, FIRST_PAYMENT_DAYS);
        assert_eq!(
            notice.pauses_at,
            Some(first_payment_deadline(now).and_utc())
        );
    }

    // ---- billingGate.server.test.ts

    #[tokio::test]
    async fn a_failed_billing_read_is_an_error_not_a_missing_row() {
        // Required since January: a company without a row would be paused.
        let store = Memory::default();
        store
            .required_since
            .borrow_mut()
            .insert(1, at("2026-01-01 00:00:00"));
        store.fail_reads.set(true);
        let result = evaluate_gate(&store, &FakeStripe::default(), 1, 10, after_window()).await;
        assert!(result.is_err());
    }

    // ---- settleCompanyBilling

    #[tokio::test]
    async fn settling_rechecks_a_company_past_its_window() {
        let store = asked_to_pay(Some(checkout_never_completed()));
        let stripe = FakeStripe {
            refresh: paid,
            ..FakeStripe::default()
        };
        let during = required_since() + Duration::days(1);
        let unchanged = settle_company_billing(&store, &stripe, 1, Some(required_since()), during)
            .await
            .unwrap();
        assert_eq!(unchanged.unwrap().payment_status, PaymentStatus::None);
        assert_eq!(stripe.refreshes.get(), 0);
        let settled =
            settle_company_billing(&store, &stripe, 1, Some(required_since()), after_window())
                .await
                .unwrap();
        assert_eq!(settled.unwrap().payment_status, PaymentStatus::Active);
        assert_eq!(stripe.locks.get(), 2);
    }

    #[tokio::test]
    async fn settling_suspends_an_overdue_company_stripe_confirms_unpaid() {
        let store = with_record(past_due());
        let settled =
            settle_company_billing(&store, &live("past_due", 4900), 1, None, after_grace())
                .await
                .unwrap();
        assert_eq!(settled.unwrap().payment_status, PaymentStatus::Suspended);
        assert_eq!(store.record(1).payment_status, PaymentStatus::Suspended);
    }

    // ---- billingGate.server.ts throttles

    #[test]
    fn suspended_and_never_paid_companies_refresh_every_minute() {
        let record = |payment_status| CompanyBilling {
            payment_status,
            ..CompanyBilling::empty(1)
        };
        assert_eq!(refresh_every_secs(&record(PaymentStatus::Suspended)), 60);
        assert_eq!(refresh_every_secs(&record(PaymentStatus::None)), 60);
        assert_eq!(refresh_every_secs(&record(PaymentStatus::PastDue)), 15 * 60);
    }

    #[test]
    fn a_check_is_due_again_after_its_interval() {
        // A company id no other test uses: the throttles are process-wide.
        let record = CompanyBilling {
            company_id: 990_001,
            ..CompanyBilling::empty(990_001)
        };
        let now = at("2026-10-01 00:00:00");
        assert!(stripe_check_due("test", &record, now));
        assert!(take_due(&LAST_REFRESH, "test", record.company_id, now, 60));
        assert!(!stripe_check_due(
            "test",
            &record,
            now + Duration::seconds(30)
        ));
        assert!(stripe_check_due(
            "test",
            &record,
            now + Duration::seconds(60)
        ));
    }
}
