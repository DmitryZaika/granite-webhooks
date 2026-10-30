//! Paying to restore a suspended company.
//!
//! A new subscription is charged to a card the manager picks, confirmed in
//! the browser with Stripe.js, then checked here.

use super::lock::{CompanyLock, LockError};
use super::machine::apply_subscription_change;
use super::store::{BillingStore, MySqlBillingStore};
use super::stripe::{Stripe, StripeError, stripe_id, subscription_snapshot};
use super::{
    BillingPlan, CompanyBilling, PaymentStatus, is_paying_subscription_status,
    needs_new_subscription, parse_billing_plans,
};
use crate::error::ApiError;
use chrono::NaiveDateTime;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const FINISH_POLLS: usize = 6;
/// How long a price read from Stripe is reused.
const PRICE_CACHE: Duration = Duration::from_mins(10);

pub const NOT_CONFIGURED: &str = "Online billing is not set up yet. Please contact support.";
const NO_BILLING_ACCOUNT: &str = "There is no billing account for this company yet.";
/// Self-serve plans start again through a plan invite, never here.
pub const SELF_SERVE_ENDED: &str =
    "This company is on a self-serve plan. Contact Granite Manager to start again.";

/// Self-serve companies change their plan and payment methods in the
/// payments app, never through the legacy plan change or the Stripe portal.
pub const SELF_SERVE_BILLING: &str =
    "This company's plan and payment methods are managed on your payments account page.";
/// The error code of [`SELF_SERVE_BILLING`].
pub const SELF_SERVE_BILLING_CODE: &str = "self_serve_billing";

/// Monthly cents per (Stripe base, price id), `None` when Stripe could not
/// say, with when the entry expires (per Lambda container).
static PRICES: LazyLock<Mutex<PriceCache>> = LazyLock::new(Mutex::default);
type PriceCache = HashMap<(String, String), (Option<i64>, Instant)>;

/// Serializes a company's billing writes (`store.withCompanyLock` in the CRM):
/// the MySQL store takes the company's named lock.
#[allow(async_fn_in_trait)] // used with concrete types only
pub trait CompanyLocking {
    /// The held lock; `None` for stores that need none.
    async fn lock_company(&self, company_id: i32) -> Result<Option<CompanyLock>, LockError>;
}

impl CompanyLocking for MySqlBillingStore<'_> {
    async fn lock_company(&self, company_id: i32) -> Result<Option<CompanyLock>, LockError> {
        Ok(Some(CompanyLock::acquire(self.0, company_id).await?))
    }
}

/// Runs `work` under the company's lock.
pub(super) async fn locked<S, T, E, F>(store: &S, company_id: i32, work: F) -> Result<T, E>
where
    S: CompanyLocking,
    E: From<LockError>,
    F: Future<Output = Result<T, E>>,
{
    let lock = store.lock_company(company_id).await?;
    let result = work.await;
    if let Some(lock) = lock {
        lock.release().await;
    }
    result
}

/// The plans from the environment.
pub fn configured_plans() -> Vec<BillingPlan> {
    parse_billing_plans(
        std::env::var("STRIPE_BILLING_PLANS").ok().as_deref(),
        std::env::var("STRIPE_PRICE_ID").ok().as_deref(),
    )
}

/// A price's monthly amount, when it is an active, plain monthly price.
pub fn monthly_cents_of(price: &Value) -> Option<i64> {
    let recurring = &price["recurring"];
    if price["active"] != true
        || recurring["interval"] != "month"
        || recurring["interval_count"].as_i64() != Some(1)
    {
        return None;
    }
    price["unit_amount"].as_i64()
}

async fn stripe_monthly_cents(stripe: &Stripe, price_id: &str) -> Option<i64> {
    let key = (stripe.base_url().to_string(), price_id.to_string());
    let cached = PRICES
        .lock()
        .ok()
        .and_then(|prices| prices.get(&key).copied())
        .filter(|(_, expires)| *expires > Instant::now());
    if let Some((cents, _)) = cached {
        return cents;
    }
    let cents = stripe
        .price(price_id)
        .await
        .ok()
        .as_ref()
        .and_then(monthly_cents_of);
    if let Ok(mut prices) = PRICES.lock() {
        prices.insert(key, (cents, Instant::now() + PRICE_CACHE));
    }
    cents
}

/// The plans with the amount Stripe will actually charge; the configured
/// cents are only a fallback when Stripe cannot be read.
pub async fn load_billing_plans(stripe: &Stripe, plans: &[BillingPlan]) -> Vec<BillingPlan> {
    if !stripe.is_enabled() {
        return plans.to_vec();
    }
    let mut priced = Vec::with_capacity(plans.len());
    for plan in plans {
        let cents = stripe_monthly_cents(stripe, &plan.price_id).await;
        priced.push(BillingPlan {
            monthly_price_cents: cents.or(plan.monthly_price_cents),
            ..plan.clone()
        });
    }
    priced
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
    /// A self-serve company: [`SELF_SERVE_BILLING`].
    #[error("{SELF_SERVE_BILLING}")]
    SelfServe,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<LockError> for ReactivationError {
    fn from(err: LockError) -> Self {
        match err {
            LockError::Busy => Self::Refused(err.to_string()),
            LockError::Database(err) => Self::Database(err),
        }
    }
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
            ReactivationError::SelfServe => Self::coded(
                axum::http::StatusCode::CONFLICT,
                SELF_SERVE_BILLING_CODE,
                SELF_SERVE_BILLING,
            ),
            ReactivationError::Database(err) => Self::Database(err),
        }
    }
}

fn refused(message: &str) -> ReactivationError {
    ReactivationError::Refused(message.into())
}

/// The subscription billing should follow: a paying one of the customer
/// (not one a plan move replaced, nor a plan invite's before its acceptance
/// put it on record), else the one on record.
pub(super) async fn current_subscription(
    stripe: &Stripe,
    record: &CompanyBilling,
) -> Result<Option<Value>, StripeError> {
    if let Some(customer) = &record.stripe_customer_id {
        let subscriptions = stripe.list_subscriptions(customer).await?;
        let on_record = |s: &Value| {
            s["id"].as_str().is_some()
                && s["id"].as_str() == record.stripe_subscription_id.as_deref()
        };
        let paying = subscriptions.iter().position(|s| {
            let set = |key: &str| s["metadata"][key].as_str().is_some_and(|id| !id.is_empty());
            !set("replaced_by")
                && (!set("plan_invite_id") || on_record(s))
                && s["status"]
                    .as_str()
                    .is_some_and(is_paying_subscription_status)
        });
        let known = subscriptions.iter().position(on_record);
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

/// The open first invoice of an unfinished reactivation for this plan, so a
/// retry confirms that payment instead of creating another subscription.
async fn pending_reactivation<'a>(
    stripe: &Stripe,
    subscriptions: &'a [Value],
    price_id: &str,
) -> Result<Option<(&'a Value, String)>, StripeError> {
    for subscription in subscriptions {
        if subscription["status"] != "incomplete"
            || subscription["items"]["data"][0]["price"]["id"].as_str() != Some(price_id)
        {
            continue;
        }
        let Some(invoice_id) = stripe_id(&subscription["latest_invoice"]) else {
            continue;
        };
        if stripe.invoice(&invoice_id).await?["status"] == "open" {
            return Ok(Some((subscription, invoice_id)));
        }
    }
    Ok(None)
}

/// The idempotency key prefix of one reactivation attempt,
/// `reactivation:<company>:<newest subscription>`: a retry sees the same
/// subscriptions, and the subscription an attempt creates starts the next.
fn attempt_key(company_id: i32, subscriptions: &[Value]) -> String {
    let newest = subscriptions
        .iter()
        .filter_map(|subscription| {
            Some((
                subscription["created"].as_i64(),
                subscription["id"].as_str()?,
            ))
        })
        .max()
        .map_or("first", |(_, id)| id);
    format!("reactivation:{company_id}:{newest}")
}

/// Starts paying for a new subscription with `payment_method_id`, under the
/// company's lock: two tabs or two managers paying at once must not end up
/// with two subscriptions (and two charges).
pub async fn start<S: BillingStore + CompanyLocking>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    payment_method_id: &str,
) -> Result<Started, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused(NOT_CONFIGURED));
    }
    if store.self_serve(company_id).await? {
        return Err(refused(SELF_SERVE_ENDED));
    }
    locked(
        store,
        company_id,
        start_locked(stripe, store, plans, company_id, payment_method_id),
    )
    .await
}

async fn start_locked<S: BillingStore>(
    stripe: &Stripe,
    store: &S,
    plans: &[BillingPlan],
    company_id: i32,
    payment_method_id: &str,
) -> Result<Started, ReactivationError> {
    let record = store.by_company(company_id).await?;
    let (Some(record), Some(customer)) = (
        record.clone(),
        record.and_then(|record| record.stripe_customer_id),
    ) else {
        return Err(refused(NO_BILLING_ACCOUNT));
    };
    let now = super::now();
    let existing = stripe.list_subscriptions(&customer).await?;
    let key = attempt_key(company_id, &existing);
    for subscription in &existing {
        if subscription_is_paid(stripe, subscription).await? {
            apply_subscription_change(store, &subscription_snapshot(subscription), now).await?;
            return Ok(Started::Paid);
        }
    }
    if !needs_new_subscription(Some(&record)) {
        return Err(refused("This company already has a subscription"));
    }
    let plan = reactivation_plan(Some(&record), plans).ok_or_else(|| refused(NOT_CONFIGURED))?;

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
            stripe
                .attach_payment_method(&method_id, &customer, &format!("{key}:attach:{method_id}"))
                .await?;
        }
    }
    stripe
        .set_default_payment_method_keyed(
            &customer,
            &method_id,
            &format!("{key}:default:{method_id}"),
        )
        .await?;

    if let Some((subscription, invoice_id)) =
        pending_reactivation(stripe, &existing, &plan.price_id).await?
    {
        let id = subscription["id"].as_str().unwrap_or_default();
        stripe
            .set_subscription_payment_method_keyed(
                id,
                &method_id,
                &format!("{key}:subscription-method:{id}:{method_id}"),
            )
            .await?;
        let invoice = stripe.invoice_with_confirmation_secret(&invoice_id).await?;
        if let Some(secret) = invoice["confirmation_secret"]["client_secret"].as_str() {
            apply_subscription_change(store, &subscription_snapshot(subscription), now).await?;
            return Ok(Started::RequiresPayment {
                client_secret: secret.to_string(),
                subscription_id: id.to_string(),
            });
        }
    }

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
            &format!("{key}:subscription:{method_id}"),
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
/// Only the writes take the company's lock, not the polling.
pub async fn finish<S: BillingStore + CompanyLocking>(
    stripe: &Stripe,
    store: &S,
    company_id: i32,
    subscription_id: Option<&str>,
) -> Result<PaymentStatus, ReactivationError> {
    if !stripe.is_enabled() {
        return Err(refused(NOT_CONFIGURED));
    }
    let record = store.by_company(company_id).await?;
    let Some(customer) = record.and_then(|record| record.stripe_customer_id) else {
        return Err(refused(NO_BILLING_ACCOUNT));
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
                locked(store, company_id, async {
                    apply_subscription_change(
                        store,
                        &subscription_snapshot(&subscription),
                        super::now(),
                    )
                    .await
                    .map_err(ReactivationError::from)
                })
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
        Some(current) if current.payment_status != PaymentStatus::Active => Some(
            locked(
                store,
                company_id,
                refresh_subscription(stripe, store, current, super::now()),
            )
            .await?,
        ),
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
    fn an_attempt_is_keyed_by_the_newest_subscription_it_saw() {
        use serde_json::json;
        assert_eq!(attempt_key(7, &[]), "reactivation:7:first");
        let seen = [
            json!({ "id": "sub_old", "created": 100 }),
            json!({ "id": "sub_newer", "created": 200 }),
        ];
        assert_eq!(attempt_key(7, &seen), "reactivation:7:sub_newer");
        let mut reversed = seen;
        reversed.reverse();
        assert_eq!(attempt_key(7, &reversed), "reactivation:7:sub_newer");
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
