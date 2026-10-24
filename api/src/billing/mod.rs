//! Stripe billing of a company's Granite Manager subscription.
//!
//! Stripe tells us about payments through webhooks; this module keeps
//! `company_billing` (one row per company), its invoices and the managers'
//! "payment required" notifications in step, and decides whether the
//! company's access is paused. A failed payment starts a 7 day grace period;
//! when it ends unpaid (Stripe confirms), the company is suspended until it
//! pays. A company asked to pay that never subscribed gets 7 days from
//! `company.billing_required_at`, then pauses until it subscribes. Companies
//! only get warnings and pauses when `company.billing_required` is on. Port of
//! the CRM's `billing*.ts` on `fix/stripe-billing`.

pub mod account;
pub mod gate;
pub mod lock;
pub mod machine;
pub mod reactivation;
pub mod reminder_emails;
pub mod reminder_store;
pub mod reminders;
pub mod setup;
pub mod store;
pub mod stripe;
#[cfg(test)]
mod test_store;

use chrono::{DateTime, Duration, NaiveDateTime, SubsecRound, Utc};
use serde::Serialize;

pub const GRACE_PERIOD_DAYS: i64 = 7;
/// Days a company has to subscribe after Stripe payment is first required.
pub const FIRST_PAYMENT_DAYS: i64 = 7;
pub const BILLING_PATH: &str = "/admin/billing";
pub const PAYMENT_REQUIRED_TITLE: &str = "Payment required";
pub const UPDATE_PAYMENT_LABEL: &str = "Update Payment Method";
/// `positions.id` of a manager: the only people billing talks to.
pub const MANAGER_POSITION: i32 = 5;

/// Whether the company is paid up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Active,
    /// A payment failed; access continues until the grace period ends.
    PastDue,
    /// Access is paused.
    Suspended,
    None,
}

impl PaymentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::PastDue => "past_due",
            Self::Suspended => "suspended",
            Self::None => "none",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "active" => Self::Active,
            "past_due" => Self::PastDue,
            "suspended" => Self::Suspended,
            _ => Self::None,
        }
    }
}

/// A company's `company_billing` row. Times are UTC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyBilling {
    pub company_id: i32,
    pub stripe_customer_id: Option<String>,
    pub stripe_subscription_id: Option<String>,
    pub stripe_price_id: Option<String>,
    pub stripe_subscription_item_id: Option<String>,
    pub stripe_payment_method_id: Option<String>,
    pub subscription_status: String,
    pub payment_status: PaymentStatus,
    pub current_period_start: Option<NaiveDateTime>,
    pub current_period_end: Option<NaiveDateTime>,
    pub last_successful_payment_at: Option<NaiveDateTime>,
    pub failed_payment_at: Option<NaiveDateTime>,
    pub grace_period_ends_at: Option<NaiveDateTime>,
    pub plan_name: Option<String>,
    pub monthly_price_cents: Option<i64>,
    pub user_count: Option<i64>,
    pub outstanding_balance_cents: i64,
    /// The `failed_payment_at` of the last dunning cycle that recovered, and
    /// when: written by the save that restores access, cleared by a new cycle.
    pub recovered_failed_at: Option<NaiveDateTime>,
    pub recovered_at: Option<NaiveDateTime>,
    /// When the first charge is due (self-serve signup).
    pub trial_end_at: Option<NaiveDateTime>,
    /// The subscription item billing users beyond the included ones.
    pub extra_user_item_id: Option<String>,
    /// The billable user count last pushed to Stripe.
    pub billable_users: Option<i64>,
    /// When the subscription is scheduled to end (a cancellation request).
    pub cancel_at: Option<NaiveDateTime>,
}

impl CompanyBilling {
    /// A company Stripe has not told us about yet.
    pub fn empty(company_id: i32) -> Self {
        Self {
            company_id,
            stripe_customer_id: None,
            stripe_subscription_id: None,
            stripe_price_id: None,
            stripe_subscription_item_id: None,
            stripe_payment_method_id: None,
            subscription_status: "incomplete".into(),
            payment_status: PaymentStatus::None,
            current_period_start: None,
            current_period_end: None,
            last_successful_payment_at: None,
            failed_payment_at: None,
            grace_period_ends_at: None,
            plan_name: None,
            monthly_price_cents: None,
            user_count: None,
            outstanding_balance_cents: 0,
            recovered_failed_at: None,
            recovered_at: None,
            trial_end_at: None,
            extra_user_item_id: None,
            billable_users: None,
            cancel_at: None,
        }
    }
}

/// A `company_billing_invoices` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingInvoice {
    pub company_id: i32,
    pub stripe_invoice_id: String,
    /// `paid` or `open`.
    pub status: String,
    pub amount_due_cents: i64,
    pub amount_paid_cents: i64,
    pub currency: String,
    pub hosted_invoice_url: Option<String>,
    pub invoice_pdf: Option<String>,
    pub paid_at: Option<NaiveDateTime>,
}

/// A "payment required" bell notification for one manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingNotification {
    pub company_id: i32,
    pub user_id: i32,
    pub dedupe_key: String,
    pub title: String,
    pub message: String,
    pub action_label: String,
    pub href: String,
}

/// What a Stripe subscription says, as far as billing cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSnapshot {
    pub id: String,
    pub customer_id: String,
    pub price_id: Option<String>,
    pub item_id: Option<String>,
    pub status: String,
    pub quantity: Option<i64>,
    pub period_start: Option<NaiveDateTime>,
    pub period_end: Option<NaiveDateTime>,
    pub payment_method_id: Option<String>,
    pub plan_name: Option<String>,
    pub unit_amount_cents: Option<i64>,
    /// `metadata.company_id`.
    pub company_id: Option<i32>,
    pub deleted: bool,
    /// When the free period ends (the first charge), while one is set.
    pub trial_end: Option<NaiveDateTime>,
    /// When the subscription is scheduled to end: `cancel_at`, or the period
    /// end with `cancel_at_period_end`.
    pub cancel_at: Option<NaiveDateTime>,
}

/// What a Stripe invoice says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvoiceSnapshot {
    pub id: String,
    pub customer_id: String,
    pub subscription_id: Option<String>,
    /// The subscription's `metadata.company_id`.
    pub company_id: Option<i32>,
    pub status: String,
    pub amount_due_cents: i64,
    pub amount_paid_cents: i64,
    pub currency: String,
    pub hosted_invoice_url: Option<String>,
    pub invoice_pdf: Option<String>,
    pub paid_at: Option<NaiveDateTime>,
    /// The invoice's subscription status read from Stripe when the event is
    /// applied; `None` when it was not read.
    pub live_subscription_status: Option<String>,
}

/// The Stripe events billing acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BillingEvent {
    CheckoutCompleted {
        id: String,
        company_id: i32,
        customer_id: String,
        subscription_id: Option<String>,
    },
    Subscription {
        id: String,
        /// `customer.subscription.created`, `.updated` or `.deleted`.
        kind: &'static str,
        subscription: SubscriptionSnapshot,
    },
    Invoice {
        id: String,
        paid: bool,
        invoice: InvoiceSnapshot,
    },
    PaymentMethodUpdated {
        id: String,
        customer_id: String,
        payment_method_id: String,
    },
}

impl BillingEvent {
    pub fn id(&self) -> &str {
        match self {
            Self::CheckoutCompleted { id, .. }
            | Self::Subscription { id, .. }
            | Self::Invoice { id, .. }
            | Self::PaymentMethodUpdated { id, .. } => id,
        }
    }

    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::CheckoutCompleted { .. } => "checkout.session.completed",
            Self::Subscription { kind, .. } => kind,
            Self::Invoice { paid: true, .. } => "invoice.paid",
            Self::Invoice { paid: false, .. } => "invoice.payment_failed",
            Self::PaymentMethodUpdated { .. } => "payment_method.updated",
        }
    }
}

/// A plan from `STRIPE_BILLING_PLANS` (`price_id|Name|cents,...`) or `STRIPE_PRICE_ID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BillingPlan {
    pub price_id: String,
    pub name: String,
    pub monthly_price_cents: Option<i64>,
}

pub fn parse_billing_plans(raw: Option<&str>, fallback_price_id: Option<&str>) -> Vec<BillingPlan> {
    let mut plans = Vec::new();
    for part in raw.unwrap_or_default().split(',') {
        let pieces: Vec<&str> = part.split('|').map(str::trim).collect();
        let price_id = pieces.first().copied().unwrap_or_default();
        if price_id.is_empty() {
            continue;
        }
        let name = pieces
            .get(1)
            .filter(|name| !name.is_empty())
            .map_or("Granite Manager", |name| name);
        let cents = pieces
            .get(2)
            .filter(|cents| !cents.is_empty())
            .and_then(|cents| cents.parse::<i64>().ok());
        plans.push(BillingPlan {
            price_id: price_id.to_string(),
            name: name.to_string(),
            monthly_price_cents: cents,
        });
    }
    if plans.is_empty()
        && let Some(price_id) = fallback_price_id.map(str::trim).filter(|id| !id.is_empty())
    {
        plans.push(BillingPlan {
            price_id: price_id.to_string(),
            name: "Granite Manager".into(),
            monthly_price_cents: None,
        });
    }
    plans
}

pub fn is_ended_subscription_status(status: &str) -> bool {
    matches!(status, "canceled" | "incomplete_expired")
}

pub fn is_paying_subscription_status(status: &str) -> bool {
    matches!(status, "active" | "trialing")
}

fn is_unpaid_subscription_status(status: &str) -> bool {
    matches!(
        status,
        "past_due" | "unpaid" | "canceled" | "incomplete" | "incomplete_expired" | "paused"
    )
}

/// The company can start a new Checkout subscription.
pub fn can_start_checkout(record: Option<&CompanyBilling>) -> bool {
    record
        .filter(|record| record.stripe_subscription_id.is_some())
        .is_none_or(|record| is_ended_subscription_status(&record.subscription_status))
}

/// The company has to pay for a new subscription to get back in (the pay page).
pub fn needs_new_subscription(record: Option<&CompanyBilling>) -> bool {
    record.is_some_and(|record| {
        can_start_checkout(Some(record)) || record.subscription_status == "incomplete"
    })
}

/// What Stripe says right now about a company whose grace period ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeLiveState {
    pub subscription_status: Option<String>,
    pub open_invoice_cents: i64,
}

pub fn stripe_confirms_non_payment(state: &StripeLiveState) -> bool {
    state.open_invoice_cents > 0
        || state
            .subscription_status
            .as_deref()
            .is_some_and(is_unpaid_subscription_status)
}

/// No successful payment yet: no billing row, or a Stripe customer whose
/// Checkout or first payment never completed.
pub fn is_awaiting_first_payment(record: Option<&CompanyBilling>) -> bool {
    record.is_none_or(|record| record.payment_status == PaymentStatus::None)
}

/// A failed or paused payment (the gate checks Stripe for these).
pub const fn is_overdue(record: &CompanyBilling) -> bool {
    matches!(
        record.payment_status,
        PaymentStatus::PastDue | PaymentStatus::Suspended
    )
}

pub fn first_payment_deadline(required_since: NaiveDateTime) -> NaiveDateTime {
    required_since + Duration::days(FIRST_PAYMENT_DAYS)
}

/// The record names another subscription that is still alive.
fn has_live_other_subscription(record: &CompanyBilling, subscription_id: Option<&str>) -> bool {
    match (subscription_id, record.stripe_subscription_id.as_deref()) {
        (Some(incoming), Some(current)) if incoming != current => {
            !is_ended_subscription_status(&record.subscription_status)
        }
        _ => false,
    }
}

pub fn add_grace_days(failed_at: NaiveDateTime) -> NaiveDateTime {
    failed_at + Duration::days(GRACE_PERIOD_DAYS)
}

/// Whole days left (rounded up); 0 once the grace period is over.
pub fn days_remaining(grace_ends_at: NaiveDateTime, now: NaiveDateTime) -> i64 {
    let remaining = (grace_ends_at - now).num_milliseconds();
    if remaining <= 0 {
        return 0;
    }
    let day = Duration::days(1).num_milliseconds();
    (remaining + day - 1) / day
}

/// `September 11, 2026`.
pub fn format_billing_date(date: NaiveDateTime) -> String {
    date.format("%B %-d, %Y").to_string()
}

pub fn payment_required_message(grace_ends_at: NaiveDateTime) -> String {
    format!(
        "Your Granite Manager subscription payment was unsuccessful. Please update your payment method or complete the payment. Your account will remain active until {}.",
        format_billing_date(grace_ends_at)
    )
}

/// A past-due record whose grace period has ended counts as suspended.
pub fn with_grace_expiry(record: &CompanyBilling, now: NaiveDateTime) -> CompanyBilling {
    let mut record = record.clone();
    if record.payment_status == PaymentStatus::PastDue
        && record.grace_period_ends_at.is_some_and(|ends| now >= ends)
    {
        record.payment_status = PaymentStatus::Suspended;
    }
    record
}

pub fn outstanding_balance_cents(invoices: &[BillingInvoice]) -> i64 {
    invoices
        .iter()
        .filter(|invoice| invoice.status == "open")
        .map(|invoice| invoice.amount_due_cents)
        .sum()
}

/// The current time, whole seconds (what a DATETIME column keeps).
pub fn now() -> NaiveDateTime {
    Utc::now().naive_utc().trunc_subsecs(0)
}

/// `2026-09-01T12:00:00.000Z`, as the CRM keyed notifications.
pub fn iso(date: NaiveDateTime) -> String {
    DateTime::<Utc>::from_naive_utc_and_offset(date, Utc)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn grace_lasts_seven_days_then_suspends() {
        let failed_at = at("2026-09-01 00:00:00");
        let record = CompanyBilling {
            payment_status: PaymentStatus::PastDue,
            subscription_status: "past_due".into(),
            failed_payment_at: Some(failed_at),
            grace_period_ends_at: Some(add_grace_days(failed_at)),
            ..CompanyBilling::empty(1)
        };
        for day in 1..=7 {
            let now = failed_at + Duration::days(day - 1) + Duration::minutes(1);
            assert_eq!(
                with_grace_expiry(&record, now).payment_status,
                PaymentStatus::PastDue
            );
        }
        assert_eq!(
            with_grace_expiry(&record, add_grace_days(failed_at)).payment_status,
            PaymentStatus::Suspended
        );
        assert_eq!(
            days_remaining(add_grace_days(failed_at), at("2026-09-03 00:00:00")),
            5
        );
        assert_eq!(
            days_remaining(add_grace_days(failed_at), at("2026-09-07 23:00:00")),
            1
        );
        assert_eq!(
            days_remaining(add_grace_days(failed_at), at("2026-09-08 00:00:00")),
            0
        );
        assert_eq!(first_payment_deadline(failed_at), at("2026-09-08 00:00:00"));
    }

    #[test]
    fn checkout_only_without_a_live_subscription() {
        assert!(can_start_checkout(None));
        assert!(can_start_checkout(Some(&CompanyBilling::empty(1))));
        let live = CompanyBilling {
            stripe_subscription_id: Some("sub_1".into()),
            ..CompanyBilling::empty(1)
        };
        let with = |status: &str| CompanyBilling {
            subscription_status: status.into(),
            ..live.clone()
        };
        assert!(!can_start_checkout(Some(&with("active"))));
        assert!(!can_start_checkout(Some(&with("past_due"))));
        assert!(can_start_checkout(Some(&with("canceled"))));
        assert!(can_start_checkout(Some(&with("incomplete_expired"))));

        assert!(!needs_new_subscription(None));
        assert!(needs_new_subscription(Some(&with("canceled"))));
        assert!(needs_new_subscription(Some(&with("incomplete"))));
        assert!(!needs_new_subscription(Some(&with("unpaid"))));
        assert!(!needs_new_subscription(Some(&with("active"))));
    }

    #[test]
    fn plans_parse_from_the_environment_format() {
        assert_eq!(
            parse_billing_plans(Some("price_a|Pro|30000, price_b||,|x"), Some("price_z")),
            vec![
                BillingPlan {
                    price_id: "price_a".into(),
                    name: "Pro".into(),
                    monthly_price_cents: Some(30000)
                },
                BillingPlan {
                    price_id: "price_b".into(),
                    name: "Granite Manager".into(),
                    monthly_price_cents: None
                },
            ]
        );
        assert_eq!(
            parse_billing_plans(None, Some(" price_z ")),
            vec![BillingPlan {
                price_id: "price_z".into(),
                name: "Granite Manager".into(),
                monthly_price_cents: None
            }]
        );
        assert_eq!(parse_billing_plans(Some(""), None), []);
    }

    #[test]
    fn messages_and_keys_match_the_crm() {
        let ends = at("2026-09-11 12:00:00");
        assert_eq!(
            payment_required_message(ends),
            "Your Granite Manager subscription payment was unsuccessful. Please update your payment method or complete the payment. Your account will remain active until September 11, 2026."
        );
        assert_eq!(iso(at("2026-09-01 12:00:00")), "2026-09-01T12:00:00.000Z");
    }

    #[test]
    fn non_payment_needs_an_open_invoice_or_an_unpaid_status() {
        let state = |status: Option<&str>, cents| StripeLiveState {
            subscription_status: status.map(str::to_string),
            open_invoice_cents: cents,
        };
        assert!(stripe_confirms_non_payment(&state(Some("active"), 4900)));
        assert!(stripe_confirms_non_payment(&state(Some("past_due"), 0)));
        assert!(!stripe_confirms_non_payment(&state(Some("active"), 0)));
        assert!(!stripe_confirms_non_payment(&state(None, 0)));
    }
}
