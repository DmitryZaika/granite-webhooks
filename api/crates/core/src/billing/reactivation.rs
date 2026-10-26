//! Paying to restore a suspended company.
//!
//! A new subscription is charged to a card the manager picks, confirmed in
//! the browser with Stripe.js, then checked here. Also Stripe as the access
//! gate asks it (`LiveGate`).
// Generic over the store, like `machine`.
#![allow(clippy::future_not_send)]

use super::machine::{
    BillingNotice, GateStripe, NO_BILLING_NOTICE, apply_subscription_change, evaluate_gate,
};
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{Stripe, StripeError, stripe_id, subscription_snapshot};
use super::{
    BillingPlan, CompanyBilling, PaymentStatus, StripeLiveState, is_paying_subscription_status,
    needs_new_subscription, parse_billing_plans,
};
use crate::error::ApiError;
use chrono::NaiveDateTime;
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const PAST_DUE_REFRESH_SECS: i64 = 15 * 60;
const SUSPENDED_REFRESH_SECS: i64 = 60;
const LIVE_CHECK_RETRY_SECS: i64 = 5 * 60;
const FINISH_POLLS: usize = 6;

/// When each company was last refreshed from / checked with Stripe (per
/// Lambda container, as the CRM kept it per server process).
static LAST_REFRESH: LazyLock<Mutex<HashMap<i32, NaiveDateTime>>> = LazyLock::new(Mutex::default);
static LAST_LIVE_CHECK: LazyLock<Mutex<HashMap<i32, NaiveDateTime>>> =
    LazyLock::new(Mutex::default);

/// The plans from the environment.
pub fn configured_plans() -> Vec<BillingPlan> {
    parse_billing_plans(
        std::env::var("STRIPE_BILLING_PLANS").ok().as_deref(),
        std::env::var("STRIPE_PRICE_ID").ok().as_deref(),
    )
}

/// What a restored company pays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactivationPlan {
    pub price_id: String,
    pub plan_name: String,
    pub quantity: i64,
    pub monthly_cents: Option<i64>,
}

/// The company's old plan when still offered, else the first one; one unit
/// (a flat price per company).
pub fn reactivation_plan(
    record: Option<&CompanyBilling>,
    plans: &[BillingPlan],
) -> Option<ReactivationPlan> {
    let old_price = record.and_then(|record| record.stripe_price_id.as_deref());
    let plan = plans
        .iter()
        .find(|plan| Some(plan.price_id.as_str()) == old_price)
        .or_else(|| plans.first())?;
    let quantity = 1;
    // Without a configured price, the old subscription's unit price: its
    // stored total may cover several per-user units from before flat pricing.
    let previous_units = record
        .and_then(|record| record.user_count)
        .filter(|count| *count > 0)
        .unwrap_or(1);
    let previous_unit_cents = record
        .filter(|record| record.stripe_price_id.as_deref() == Some(plan.price_id.as_str()))
        .and_then(|record| record.monthly_price_cents)
        .map(|cents| (cents * 2 + previous_units).div_euclid(previous_units * 2));
    Some(ReactivationPlan {
        price_id: plan.price_id.clone(),
        plan_name: plan.name.clone(),
        quantity,
        monthly_cents: plan
            .monthly_price_cents
            .or(previous_unit_cents)
            .map(|cents| cents * quantity),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ReactivationError {
    /// For the user (Stripe's own words, or why the payment cannot start).
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<StripeError> for ReactivationError {
    fn from(err: StripeError) -> Self {
        Self::Refused(err.to_string())
    }
}

impl From<ReactivationError> for ApiError {
    fn from(err: ReactivationError) -> Self {
        match err {
            ReactivationError::Refused(message) => Self::BadRequest(message),
            ReactivationError::Database(err) => Self::Database(err),
        }
    }
}

fn refused(message: &str) -> ReactivationError {
    ReactivationError::Refused(message.into())
}

/// The subscription billing should follow: a paying one of the customer,
/// else the one on record.
async fn current_subscription(
    stripe: &Stripe,
    record: &CompanyBilling,
) -> Result<Option<Value>, StripeError> {
    if let Some(customer) = &record.stripe_customer_id {
        let subscriptions = stripe.list_subscriptions(customer).await?;
        let paying = subscriptions.iter().position(|s| {
            s["status"]
                .as_str()
                .is_some_and(is_paying_subscription_status)
        });
        let known = subscriptions.iter().position(|s| {
            s["id"].as_str().is_some()
                && s["id"].as_str() == record.stripe_subscription_id.as_deref()
        });
        if let Some(index) = paying.or(known) {
            return Ok(subscriptions.into_iter().nth(index));
        }
    }
    match &record.stripe_subscription_id {
        Some(id) => stripe.subscription(id).await.map(Some),
        None => Ok(None),
    }
}

/// The record after applying Stripe's current subscription.
pub(super) async fn refresh_subscription<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    record: CompanyBilling,
    now: NaiveDateTime,
) -> Result<CompanyBilling, ReactivationError> {
    if !stripe.is_enabled() {
        return Ok(record);
    }
    let Some(subscription) = current_subscription(stripe, &record).await? else {
        return Ok(record);
    };
    apply_subscription_change(store, &subscription_snapshot(&subscription), now).await?;
    Ok(store.by_company(record.company_id).await?.unwrap_or(record))
}

/// Stripe for the access gate, throttled per company.
pub struct LiveGate<'a> {
    pub stripe: &'a Stripe,
    pub pool: &'a MySqlPool,
    pub now: NaiveDateTime,
}

fn due(
    map: &Mutex<HashMap<i32, NaiveDateTime>>,
    company_id: i32,
    now: NaiveDateTime,
    every_secs: i64,
) -> bool {
    let mut last = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last
        .get(&company_id)
        .is_some_and(|at| (now - *at).num_seconds() < every_secs)
    {
        return false;
    }
    last.insert(company_id, now);
    true
}

impl GateStripe for LiveGate<'_> {
    async fn refresh(&self, record: CompanyBilling) -> CompanyBilling {
        let every = if record.payment_status == PaymentStatus::Suspended {
            SUSPENDED_REFRESH_SECS
        } else {
            PAST_DUE_REFRESH_SECS
        };
        if !due(&LAST_REFRESH, record.company_id, self.now, every) {
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
            || !due(
                &LAST_LIVE_CHECK,
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
}

/// The access gate for a page load; errors keep access (like the CRM).
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
            tracing::warn!(error = %err, "billing gate failed");
            NO_BILLING_NOTICE
        }
    }
}

/// How the payment starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// Nothing to pay: the company is paid up.
    Paid,
    /// Confirm the first invoice's payment in the browser, then finish.
    RequiresPayment {
        client_secret: String,
        subscription_id: String,
    },
}

async fn subscription_is_paid(stripe: &Stripe, subscription: &Value) -> Result<bool, StripeError> {
    let status = subscription["status"].as_str().unwrap_or_default();
    if is_paying_subscription_status(status) {
        return Ok(true);
    }
    if status != "incomplete" {
        return Ok(false);
    }
    let Some(invoice_id) = stripe_id(&subscription["latest_invoice"]) else {
        return Ok(false);
    };
    Ok(stripe.invoice(&invoice_id).await?["status"] == "paid")
}

/// Starts paying for a new subscription with `payment_method_id`.
pub async fn start<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    payment_method_id: &str,
) -> Result<Started, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused("Billing is not configured"));
    }
    let record = store.by_company(company_id).await?;
    let (Some(record), Some(customer)) = (
        record.clone(),
        record.and_then(|record| record.stripe_customer_id),
    ) else {
        return Err(refused("No Stripe customer for this company"));
    };
    let now = super::now();
    let existing = stripe.list_subscriptions(&customer).await?;
    for subscription in &existing {
        if subscription_is_paid(stripe, subscription).await? {
            apply_subscription_change(store, &subscription_snapshot(subscription), now).await?;
            return Ok(Started::Paid);
        }
    }
    if !needs_new_subscription(Some(&record)) {
        return Err(refused("This company already has a subscription"));
    }
    let plan = reactivation_plan(Some(&record), plans)
        .ok_or_else(|| refused("Billing plans are not configured"))?;

    let method = stripe.payment_method(payment_method_id).await?;
    if method["type"] != "card" {
        return Err(refused("Please use a credit or debit card"));
    }
    let method_id = method["id"]
        .as_str()
        .unwrap_or(payment_method_id)
        .to_string();
    match stripe_id(&method["customer"]) {
        Some(owner) if owner != customer => return Err(refused("This card cannot be used")),
        Some(_) => {}
        None => {
            stripe.attach_payment_method(&method_id, &customer).await?;
        }
    }
    stripe
        .set_default_payment_method(&customer, &method_id)
        .await?;
    for subscription in &existing {
        if subscription["status"] == "incomplete"
            && let Some(id) = subscription["id"].as_str()
        {
            stripe.cancel_subscription(id).await?;
        }
    }
    let subscription = stripe
        .create_subscription(
            &customer,
            &plan.price_id,
            plan.quantity,
            &method_id,
            company_id,
        )
        .await?;
    apply_subscription_change(store, &subscription_snapshot(&subscription), now).await?;
    let invoice = &subscription["latest_invoice"];
    if invoice.is_object() {
        if invoice["status"] == "paid" {
            return Ok(Started::Paid);
        }
        if let (Some(secret), Some(id)) = (
            invoice["confirmation_secret"]["client_secret"].as_str(),
            subscription["id"].as_str(),
        ) {
            return Ok(Started::RequiresPayment {
                client_secret: secret.to_string(),
                subscription_id: id.to_string(),
            });
        }
    }
    Err(refused("Stripe did not return a payment to confirm"))
}

/// After the browser confirmed the payment: waits (briefly) for the
/// subscription to be paid and applies it. `Active` once access is back.
pub async fn finish<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    company_id: i32,
    subscription_id: Option<&str>,
) -> Result<PaymentStatus, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused("Billing is not configured"));
    }
    let record = store.by_company(company_id).await?;
    let Some(customer) = record.and_then(|record| record.stripe_customer_id) else {
        return Err(refused("No Stripe customer for this company"));
    };
    if let Some(subscription_id) = subscription_id {
        for attempt in 0..FINISH_POLLS {
            let subscription = stripe.subscription(subscription_id).await?;
            if stripe_id(&subscription["customer"]).as_deref() != Some(customer.as_str()) {
                return Err(refused("This subscription does not belong to your company"));
            }
            if subscription["status"]
                .as_str()
                .is_some_and(is_paying_subscription_status)
            {
                apply_subscription_change(
                    store,
                    &subscription_snapshot(&subscription),
                    super::now(),
                )
                .await?;
                break;
            }
            if attempt + 1 < FINISH_POLLS {
                tokio::time::sleep(stripe.poll_delay).await;
            }
        }
    }
    let current = store.by_company(company_id).await?;
    let refreshed = match current {
        Some(current) if current.payment_status != PaymentStatus::Active => {
            Some(refresh_subscription(stripe, store, current, super::now()).await?)
        }
        other => other,
    };
    Ok(match refreshed {
        Some(record) if record.payment_status == PaymentStatus::Active => PaymentStatus::Active,
        _ => PaymentStatus::PastDue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canceled(price: &str, user_count: Option<i64>, monthly: Option<i64>) -> CompanyBilling {
        CompanyBilling {
            stripe_price_id: Some(price.into()),
            subscription_status: "canceled".into(),
            payment_status: PaymentStatus::Suspended,
            user_count,
            monthly_price_cents: monthly,
            ..CompanyBilling::empty(1)
        }
    }

    #[test]
    fn one_unit_at_the_configured_price_whatever_the_old_user_count() {
        let plans = parse_billing_plans(Some("price_flat|Granite Manager|30000"), None);
        assert_eq!(
            reactivation_plan(
                Some(&canceled("price_flat", Some(8), Some(240_000))),
                &plans
            ),
            Some(ReactivationPlan {
                price_id: "price_flat".into(),
                plan_name: "Granite Manager".into(),
                quantity: 1,
                monthly_cents: Some(30000),
            })
        );
    }

    #[test]
    fn without_configured_cents_the_old_unit_price_not_the_old_total() {
        let plans = parse_billing_plans(Some("price_flat|Granite Manager"), None);
        let plan =
            reactivation_plan(Some(&canceled("price_flat", Some(3), Some(14700))), &plans).unwrap();
        assert_eq!(plan.quantity, 1);
        assert_eq!(plan.monthly_cents, Some(4900));
    }

    #[test]
    fn without_configured_cents_or_the_old_price_the_amount_is_unknown() {
        let plans = parse_billing_plans(Some("price_flat|Granite Manager"), None);
        let plan = reactivation_plan(Some(&canceled("price_old", None, None)), &plans).unwrap();
        assert_eq!(plan.price_id, "price_flat");
        assert_eq!(plan.monthly_cents, None);
        assert_eq!(reactivation_plan(None, &[]), None);
    }
}
