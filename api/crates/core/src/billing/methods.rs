//! Card and bank account payment methods (self-serve billing step 13).
//!
//! One saved method per company (D5): a Manager's new method becomes the
//! customer's and the subscription's default, then the old one is detached.
//! A bank account waiting for micro-deposits is kept as
//! `company_billing.pending_setup_intent_id` until Stripe verifies it. Holds
//! pause access [`HOLD_DAYS`] after they start: a blocked bank account's
//! until a new method is saved, a dispute's until no dispute of the company
//! is open or lost (every dispute is kept in `billing_disputes`; a new method
//! does not lift it, a superuser can: [`clear_hold_by_staff`]). Paying an
//! open invoice never changes `payment_status` (D17): only `invoice.paid` or
//! the gate's Stripe read do. A bank payment in flight lifts a pause once per
//! invoice: not after a bank payment of that invoice failed
//! (`billing_bank_payment_failures`), never after the invoice was paid.
//!
//! Holds apply to self-serve companies only (`plan_source = 'subscription'`):
//! a legacy company's dispute or blocked account only tells staff.
//!
//! The Stripe events of this module (`setup_intent.*`,
//! `payment_intent.processing`, `charge.failed`, `charge.dispute.created`,
//! `charge.dispute.closed`, `payment_method.automatically_updated`) are
//! claimed once by event id and re-read from Stripe before anything changes,
//! so a late or out-of-order event acts on Stripe's current state (a closed
//! dispute's outcome is final, read from the event, and never reopened by a
//! late `created`). Logs carry Stripe error codes only.

use super::addons::{AccountPaymentMethod, payment_method};
use super::cancellation::{company_name, failure_kind, manager_contacts};
use super::reminder_store::MySqlReminderStore;
use super::reminders::{
    ClaimedReminder, ReminderKey, ReminderSendResult, ReminderStore, classify_send_failure,
    hold_plan, render_hold_email,
};
use super::lock::{CompanyLock, LockError};
use super::method_emails::{
    BankFailedInput, BankFailure, HoldOutcome, InternalEvent, InternalNotice, render_bank_failed,
    render_internal_notice,
};
use super::store::{BillingStore, Claim, MySqlBillingStore, bank_verification_until};
use super::stripe::{SetupPurpose, StripeError, positive_int, stripe_id};
use super::{
    CompanyBilling, MethodHold, MethodState, is_ended_subscription_status, is_overdue,
    payments_account_url,
};
use crate::error::{ApiError, ApiResult};
use crate::signup::intent::{self, SignupStatus};
use crate::signup::payment::{Setup, is_test_mode};
use crate::signup::provision::{ResumeOutcome, awaiting_signup, resume_verified};
use crate::state::AppState;
use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDateTime};
use lambda_http::tracing;
use serde_json::Value;

/// Days a company keeps access after its saved method stopped working
/// (blocked bank account D10, dispute D11), unless it saves a new one.
pub const HOLD_DAYS: i64 = 7;

/// Days a bank payment in flight (`payment_intent.processing`) keeps the
/// gate from pausing the company (D18). ACH takes up to 4 business days.
pub const PROCESSING_DAYS: i64 = 10;

/// The retry policy set in Stripe's Dashboard (D7), not in code.
///
/// Cards use Smart Retries over 1 week; ACH retries insufficient funds only, at most
/// 2 times within 40 days (Stripe's fixed rule); when retries end the
/// subscription is left past due (our 7-day grace then pauses access).
pub const RETRY_POLICY: &str = "Cards: Smart Retries within 1 week. ACH: insufficient funds only, up to 2 retries within 40 days. After the last retry: leave the subscription past due.";

/// `metadata[purpose]` of a SetupIntent made on the account page.
pub const PURPOSE_UPDATE: &str = "payment_method_update";
/// `metadata[purpose]` of a SetupIntent made on the pay page.
pub const PURPOSE_PAY: &str = "pay";
/// `metadata[purpose]` of a plan invite's SetupIntent.
pub const PURPOSE_PLAN_INVITE: &str = "plan_invite";

/// How long one payment method email may take (as the cancellation emails).
const EMAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// For alert email failures reported to `PostHog`.
static POSTHOG_HTTP: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(common::http::client);

/// Where internal payment notices go (`BILLING_ALERT_EMAIL`); `None` sends
/// none (D12).
pub fn alert_email(state: &AppState) -> Option<&str> {
    state.billing_alert_email.as_deref()
}

/// When a hold set at `at` pauses access.
pub fn hold_until(at: NaiveDateTime) -> NaiveDateTime {
    at + Duration::days(HOLD_DAYS)
}

/// A bank payment is on its way and still inside [`PROCESSING_DAYS`].
pub fn is_processing(methods: &MethodState, now: NaiveDateTime) -> bool {
    methods
        .payment_processing_at
        .is_some_and(|at| now < at + Duration::days(PROCESSING_DAYS))
}

/// The hold's end has come: access pauses until a new method is saved.
pub fn hold_reached_end(methods: &MethodState, now: NaiveDateTime) -> bool {
    methods.hold.is_some() && methods.hold_until.is_some_and(|until| now >= until)
}

/// The payment method's type (`card`, `us_bank_account`) and last four
/// digits, from an expanded payment method (a SetupIntent's
/// `payment_method`). Both `None` when it is not expanded.
pub fn method_type_and_last4(payment_method: &Value) -> (Option<String>, Option<String>) {
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let kind = text(&payment_method["type"]);
    let last4 = kind
        .as_deref()
        .and_then(|kind| text(&payment_method[kind]["last4"]));
    (kind, last4)
}

/// The SetupIntent waits for micro-deposits.
pub fn is_microdeposits(setup_intent: &Value) -> bool {
    setup_intent["status"] == "requires_action"
        && setup_intent["next_action"]["type"] == "verify_with_microdeposits"
}

/// Stripe's page where the micro-deposit code is entered.
pub fn verify_url(setup_intent: &Value) -> Option<String> {
    setup_intent["next_action"]["verify_with_microdeposits"]["hosted_verification_url"]
        .as_str()
        .map(str::to_string)
}

/// When the micro-deposits must be verified: the signup's clock
/// (`signup::payment::bank_verification_due`), one for every bank account.
pub fn verify_by(setup_intent: &Value) -> Option<NaiveDateTime> {
    crate::signup::payment::bank_verification_due(setup_intent).map(|due| due.naive_utc())
}

/// A stable UUID-shaped attempt id for a webhook's payment (the same event
/// always pays under the same idempotency key).
pub fn attempt_for_event(event_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(event_id.as_bytes());
    let hex = digest.iter().take(16).fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    });
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// What a SetupIntent did to the company.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The method is the company's default now (or the signup waiting on its
    /// bank completed with it).
    Saved {
        payment_method: Option<AccountPaymentMethod>,
    },
    /// A bank account waiting for micro-deposits; the current method stays.
    Verifying {
        payment_method: Option<AccountPaymentMethod>,
        verify_url: Option<String>,
        verify_by: Option<NaiveDateTime>,
    },
    /// Nothing usable (declined, cancelled, or not confirmed).
    Failed,
}

fn saved(setup_intent: &Value) -> Applied {
    Applied::Saved {
        payment_method: payment_method(&setup_intent["payment_method"]),
    }
}

fn verifying(setup_intent: &Value) -> Applied {
    Applied::Verifying {
        payment_method: payment_method(&setup_intent["payment_method"]),
        verify_url: verify_url(setup_intent),
        verify_by: verify_by(setup_intent),
    }
}

/// 404 `setup_not_found`: not a SetupIntent of the company.
pub fn setup_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "setup_not_found",
        "This payment method setup was not found. Start again.",
    )
}

/// 409 `no_customer`: the company has no Stripe customer.
pub fn no_customer() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "no_customer",
        "Your company has no billing account yet. Contact support@granite-manager.com.",
    )
}

/// 409 `no_payment_method`: nothing saved to pay with.
pub fn no_payment_method() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "no_payment_method",
        "Add a payment method first.",
    )
}

/// A Stripe failure for the caller: 503 `payments_unavailable` when Stripe
/// could not answer, else a 502. Logs the code only.
pub fn stripe_failure(err: &StripeError, company_id: i32, what: &'static str) -> ApiError {
    tracing::warn!(company_id, stripe_code = err.code(), what, "Stripe payment method call failed");
    if err.is_retryable() || matches!(err, StripeError::NotConfigured) {
        crate::signup::payments_unavailable()
    } else {
        ApiError::Upstream("We could not reach our payment provider. Try again.".into())
    }
}

fn lock_failure(err: LockError) -> ApiError {
    match err {
        LockError::Busy => ApiError::Unavailable(super::lock::BUSY_MESSAGE.into()),
        LockError::Database(err) => ApiError::Database(err),
    }
}

/// The SetupIntent belongs to the company: its `metadata.company_id` and
/// customer.
fn belongs_to(record: Option<&CompanyBilling>, company_id: i32, setup_intent: &Value) -> bool {
    let customer = stripe_id(&setup_intent["customer"]);
    positive_int(&setup_intent["metadata"]["company_id"]) == Some(company_id)
        && record
            .and_then(|record| record.stripe_customer_id.as_deref())
            .is_some_and(|stored| customer.as_deref() == Some(stored))
}

/// Applies a SetupIntent read from Stripe to the company (D5, D10, D11).
///
/// `succeeded`: a signup waiting on its bank resumes with it (without the
/// company lock); else, under the lock, it becomes the customer's and the
/// live subscription's default, the previous method is detached, holds and
/// a pending bank account are cleared. Micro-deposits pending: kept as the
/// company's pending SetupIntent (a different pending one is cancelled).
/// Anything else is `Failed`. Idempotent. 404 `setup_not_found` when the
/// SetupIntent is not the company's.
pub async fn apply_setup(
    state: &AppState,
    company_id: i32,
    setup_intent: &Value,
) -> ApiResult<Applied> {
    let store = MySqlBillingStore(&state.pool);
    let record = store.by_company(company_id).await?;
    if !belongs_to(record.as_ref(), company_id, setup_intent) {
        return Err(setup_not_found());
    }
    let Some(setup_id) = setup_intent["id"].as_str().filter(|id| !id.is_empty()) else {
        return Err(setup_not_found());
    };
    if setup_intent["status"] == "succeeded" {
        let Some(method) = stripe_id(&setup_intent["payment_method"]) else {
            return Ok(Applied::Failed);
        };
        if let Some(signup) = awaiting_signup(&state.pool, company_id).await? {
            match resume_verified(state, &signup.id, setup_intent).await? {
                ResumeOutcome::Completed => {
                    clear_pending(state, company_id, setup_id).await?;
                    return Ok(saved(setup_intent));
                }
                ResumeOutcome::StillVerifying => return Ok(verifying(setup_intent)),
                ResumeOutcome::NotAwaiting => {}
            }
        }
        let lock = CompanyLock::acquire(&state.pool, company_id)
            .await
            .map_err(lock_failure)?;
        let made = make_default(state, company_id, setup_id, &method).await;
        lock.release().await;
        made?;
        return Ok(saved(setup_intent));
    }
    if is_microdeposits(setup_intent) {
        let lock = CompanyLock::acquire(&state.pool, company_id)
            .await
            .map_err(lock_failure)?;
        let stored = store_pending(state, company_id, setup_id).await;
        lock.release().await;
        stored?;
        return Ok(verifying(setup_intent));
    }
    Ok(Applied::Failed)
}

/// Under the company lock: `method` becomes the default everywhere, the old
/// one is detached, a blocked-account hold and the pending bank account are
/// cleared (a dispute hold stays: only the disputes end it).
async fn make_default(
    state: &AppState,
    company_id: i32,
    setup_id: &str,
    method: &str,
) -> ApiResult<()> {
    let record = MySqlBillingStore(&state.pool)
        .by_company(company_id)
        .await?
        .ok_or_else(setup_not_found)?;
    let pending = record.methods.pending_setup_intent_id.as_deref();
    // Applied already (a repeat, or the webhook first): nothing to redo, and
    // a hold set since is not cleared by the same method.
    if record.stripe_payment_method_id.as_deref() == Some(method) && pending != Some(setup_id) {
        return Ok(());
    }
    let customer = record
        .stripe_customer_id
        .as_deref()
        .ok_or_else(setup_not_found)?;
    let stripe = &state.stripe;
    stripe
        .set_default_payment_method_keyed(
            customer,
            method,
            &format!("pm-default:{setup_id}:customer"),
        )
        .await
        .map_err(|err| stripe_failure(&err, company_id, "default payment method"))?;
    if let Some(subscription) = record
        .stripe_subscription_id
        .as_deref()
        .filter(|_| !is_ended_subscription_status(&record.subscription_status))
    {
        match stripe
            .set_subscription_payment_method_keyed(
                subscription,
                method,
                &format!("pm-default:{setup_id}:subscription"),
            )
            .await
        {
            Ok(_) | Err(StripeError::Missing(_)) => {}
            Err(err) => return Err(stripe_failure(&err, company_id, "subscription payment method")),
        }
    }
    if let Some(old) = record
        .stripe_payment_method_id
        .as_deref()
        .filter(|old| *old != method)
    {
        match stripe
            .detach_payment_method(old, &format!("pm-detach:{setup_id}:{old}"))
            .await
        {
            Ok(_) | Err(StripeError::Missing(_)) => {}
            Err(err) if err.is_retryable() => {
                return Err(stripe_failure(&err, company_id, "detach payment method"));
            }
            // Already detached or never attached: the new method stands.
            Err(err) => {
                tracing::warn!(company_id, stripe_code = err.code(), "old payment method not detached");
            }
        }
    }
    if let Some(other) = pending.filter(|other| *other != setup_id) {
        cancel_quietly(state, company_id, other, setup_id).await;
    }
    // MySQL assigns left to right: `method_hold` last, so the IFs see the
    // old value.
    sqlx::query(
        "UPDATE company_billing
            SET stripe_payment_method_id = ?, pending_setup_intent_id = NULL,
                method_hold_at = IF(method_hold = 'disputed', method_hold_at, NULL),
                method_hold_until = IF(method_hold = 'disputed', method_hold_until, NULL),
                method_hold_charge_id = IF(method_hold = 'disputed', method_hold_charge_id, NULL),
                method_hold_amount_cents =
                  IF(method_hold = 'disputed', method_hold_amount_cents, NULL),
                method_hold_charged_at = IF(method_hold = 'disputed', method_hold_charged_at, NULL),
                method_hold = IF(method_hold = 'disputed', method_hold, NULL)
          WHERE company_id = ?",
    )
    .bind(method)
    .bind(company_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Under the company lock: the bank account waiting for micro-deposits.
async fn store_pending(state: &AppState, company_id: i32, setup_id: &str) -> ApiResult<()> {
    let record = MySqlBillingStore(&state.pool).by_company(company_id).await?;
    if let Some(other) = record
        .as_ref()
        .and_then(|record| record.methods.pending_setup_intent_id.as_deref())
        .filter(|other| *other != setup_id)
    {
        cancel_quietly(state, company_id, other, setup_id).await;
    }
    sqlx::query("UPDATE company_billing SET pending_setup_intent_id = ? WHERE company_id = ?")
        .bind(setup_id)
        .bind(company_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// Cancels a replaced pending SetupIntent, best effort.
async fn cancel_quietly(state: &AppState, company_id: i32, pending: &str, replaced_by: &str) {
    if let Err(err) = state
        .stripe
        .cancel_setup_intent(pending, &format!("pm-cancel:{replaced_by}:{pending}"))
        .await
        && !matches!(err, StripeError::Missing(_))
    {
        tracing::warn!(company_id, stripe_code = err.code(), "pending bank setup not cancelled");
    }
}

/// Clears the pending bank account when it is `setup_id`.
async fn clear_pending(state: &AppState, company_id: i32, setup_id: &str) -> sqlx::Result<bool> {
    let done = sqlx::query(
        "UPDATE company_billing SET pending_setup_intent_id = NULL
          WHERE company_id = ? AND pending_setup_intent_id = ?",
    )
    .bind(company_id)
    .bind(setup_id)
    .execute(&state.pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// What the account and pay pages set up a method for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupFor {
    /// The account page (`payment_method_update`).
    Update,
    /// The pay page (`pay`): Stripe's success webhook also pays an overdue
    /// invoice.
    Pay,
}

impl SetupFor {
    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Update => PURPOSE_UPDATE,
            Self::Pay => PURPOSE_PAY,
        }
    }
}

/// A SetupIntent for a signed-in Manager on the company's customer.
///
/// Key `billing-setup:<company_id>:<uuid>`. With the test seam on (a `sk_test_`
/// key and `SIGNUP_TEST_PAYMENT_METHOD`) a test card is confirmed and applied
/// here (`Setup::Test`). 409 `no_customer`, 503 `payments_unavailable`.
pub async fn company_setup(state: &AppState, company_id: i32, purpose: SetupFor) -> ApiResult<Setup> {
    let record = MySqlBillingStore(&state.pool).by_company(company_id).await?;
    let customer = record
        .as_ref()
        .and_then(|record| record.stripe_customer_id.clone())
        .ok_or_else(no_customer)?;
    let key = format!("billing-setup:{company_id}:{}", uuid::Uuid::new_v4());
    if is_test_mode(state)
        && let Some(method) = state.signup.test_payment_method.as_deref()
    {
        let created = state
            .stripe
            .create_confirmed_test_setup_intent(
                &customer,
                SetupPurpose::Company {
                    company_id,
                    purpose: purpose.purpose(),
                    idempotency_key: &key,
                },
                method,
            )
            .await
            .map_err(|err| stripe_failure(&err, company_id, "test setup"))?;
        return match apply_setup(state, company_id, &created).await? {
            Applied::Saved { .. } => Ok(Setup::Test),
            _ => Err(ApiError::Upstream(
                "The test payment method was not accepted.".into(),
            )),
        };
    }
    let Some(publishable_key) = state.signup.publishable_key.as_deref() else {
        return Err(crate::signup::payments_unavailable());
    };
    let created = state
        .stripe
        .create_company_setup_intent(&customer, company_id, purpose.purpose(), None, &key)
        .await
        .map_err(|err| stripe_failure(&err, company_id, "setup intent"))?;
    let client_secret = created["client_secret"]
        .as_str()
        .ok_or_else(|| ApiError::Upstream("We could not set up payments. Try again.".into()))?;
    Ok(Setup::Stripe {
        client_secret: client_secret.to_string(),
        publishable_key: publishable_key.to_string(),
    })
}

/// How paying the open invoice went. No `Debug`: a decline holds Stripe's
/// message, for the payer only.
#[derive(Clone, PartialEq, Eq)]
pub enum InvoicePayment {
    Paid { amount_cents: i64 },
    /// A bank debit is on its way (up to 4 business days).
    Processing { amount_cents: i64 },
    /// The card needs 3D Secure: `client_secret` is the invoice's
    /// confirmation secret for `stripe.handleNextAction`.
    RequiresAction {
        client_secret: String,
        amount_cents: i64,
    },
    /// Stripe's decline message, shown as is to the payer, never logged.
    Declined { message: String, amount_cents: i64 },
    NothingDue,
}

fn amount_of(invoice: &Value) -> i64 {
    invoice["amount_remaining"]
        .as_i64()
        .or_else(|| invoice["amount_due"].as_i64())
        .unwrap_or_default()
}

/// The subscription an invoice bills (dahlia's
/// `parent.subscription_details.subscription`, else the legacy top-level
/// `subscription`); `None` for an invoice of no subscription.
fn invoice_subscription(invoice: &Value) -> Option<String> {
    stripe_id(&invoice["parent"]["subscription_details"]["subscription"])
        .or_else(|| stripe_id(&invoice["subscription"]))
}

/// The oldest open invoice of `subscription` (the company's current one)
/// with something left to pay: never an invoice of an old subscription or
/// one made by hand.
fn oldest_open<'a>(invoices: &'a [Value], subscription: Option<&str>) -> Option<&'a Value> {
    let subscription = subscription?;
    invoices
        .iter()
        .filter(|invoice| invoice["status"] == "open")
        .filter(|invoice| invoice_subscription(invoice).as_deref() == Some(subscription))
        .filter(|invoice| amount_of(invoice) > 0)
        .min_by_key(|invoice| {
            (
                invoice["created"].as_i64().unwrap_or(i64::MAX),
                invoice["id"].as_str().unwrap_or_default().to_string(),
            )
        })
}

/// Marks a bank payment of `invoice_id` in flight; an older mark past
/// [`PROCESSING_DAYS`] starts over. `true` when it did.
///
/// Never for an invoice already paid (`company_billing_invoices`, written by
/// `invoice.paid` before it clears the mark, so a late `processing` cannot
/// land after it), nor for one whose bank payment failed before (D18 lifts a
/// pause once per invoice: access then returns when the money arrives).
pub async fn mark_processing(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    invoice_id: &str,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let marked = sqlx::query(
        "UPDATE company_billing
            SET payment_processing_at = IF(payment_processing_at IS NULL
                                             OR payment_processing_at <= ?, ?, payment_processing_at)
          WHERE company_id = ?
            AND NOT EXISTS (SELECT 1 FROM company_billing_invoices paid
                             WHERE paid.stripe_invoice_id = ? AND paid.status = 'paid')
            AND NOT EXISTS (SELECT 1 FROM billing_bank_payment_failures failed
                             WHERE failed.stripe_invoice_id = ?)",
    )
    .bind(now - Duration::days(PROCESSING_DAYS))
    .bind(now)
    .bind(company_id)
    .bind(invoice_id)
    .bind(invoice_id)
    .execute(pool)
    .await?;
    Ok(marked.rows_affected() == 1)
}

/// A bank payment of `invoice_id` failed (`charge.failed`): its next bank
/// payment no longer lifts a pause. Idempotent.
pub async fn record_bank_failure(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    invoice_id: &str,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT IGNORE INTO billing_bank_payment_failures (stripe_invoice_id, company_id, failed_at)
         VALUES (?, ?, ?)",
    )
    .bind(invoice_id)
    .bind(company_id)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// `invoice.paid` / `invoice.payment_failed`: the payment in flight landed.
pub async fn clear_processing(pool: &sqlx::MySqlPool, company_id: i32) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE company_billing SET payment_processing_at = NULL
          WHERE company_id = ? AND payment_processing_at IS NOT NULL",
    )
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The method to charge: the stored one, else the customer's default.
async fn default_method(
    state: &AppState,
    record: &CompanyBilling,
    customer: &str,
) -> ApiResult<Option<String>> {
    if let Some(method) = &record.stripe_payment_method_id {
        return Ok(Some(method.clone()));
    }
    match state.stripe.customer_with_default_payment_method(customer).await {
        Ok(customer) => Ok(stripe_id(&customer["invoice_settings"]["default_payment_method"])),
        Err(StripeError::Missing(_)) => Ok(None),
        Err(err) => Err(stripe_failure(&err, record.company_id, "customer")),
    }
}

/// Pays the company's oldest open subscription invoice with its default
/// method (D17).
///
/// Under the key `invoice-pay:<invoice>:<attempt_id>`. Never
/// changes `payment_status`: `invoice.paid` does. A bank debit marks
/// `payment_processing_at` ([`mark_processing`]: not for an invoice whose
/// bank payment failed before). 400 when `attempt_id` is not a UUID, 409
/// `no_payment_method`, 503 `payments_unavailable`.
pub async fn pay_open_invoice(
    state: &AppState,
    company_id: i32,
    attempt_id: &str,
) -> ApiResult<InvoicePayment> {
    let attempt = uuid::Uuid::parse_str(attempt_id.trim())
        .map_err(|_| ApiError::BadRequest("attempt_id must be a UUID".into()))?
        .hyphenated()
        .to_string();
    let record = MySqlBillingStore(&state.pool)
        .by_company(company_id)
        .await?
        .ok_or_else(no_payment_method)?;
    let customer = record
        .stripe_customer_id
        .clone()
        .ok_or_else(no_payment_method)?;
    let method = default_method(state, &record, &customer)
        .await?
        .ok_or_else(no_payment_method)?;
    let invoices = state
        .stripe
        .open_invoices(&customer)
        .await
        .map_err(|err| stripe_failure(&err, company_id, "open invoices"))?;
    let Some(invoice) = oldest_open(&invoices, record.stripe_subscription_id.as_deref()) else {
        return Ok(InvoicePayment::NothingDue);
    };
    let invoice_id = invoice["id"].as_str().unwrap_or_default().to_string();
    let amount_cents = amount_of(invoice);
    let paid = state
        .stripe
        .pay_invoice_with(
            &invoice_id,
            Some(&method),
            &format!("invoice-pay:{invoice_id}:{attempt}"),
        )
        .await;
    match paid {
        Ok(answer) if answer["status"] == "paid" => Ok(InvoicePayment::Paid { amount_cents }),
        Ok(_) => {
            mark_processing(&state.pool, company_id, &invoice_id, super::now()).await?;
            Ok(InvoicePayment::Processing { amount_cents })
        }
        Err(StripeError::Api(failure)) if failure.code == "invoice_payment_intent_requires_action" => {
            let live = state
                .stripe
                .invoice_with_confirmation_secret(&invoice_id)
                .await
                .map_err(|err| stripe_failure(&err, company_id, "invoice confirmation"))?;
            let client_secret = live["confirmation_secret"]["client_secret"]
                .as_str()
                .ok_or_else(|| ApiError::Upstream("We could not take the payment. Try again.".into()))?;
            Ok(InvoicePayment::RequiresAction {
                client_secret: client_secret.to_string(),
                amount_cents,
            })
        }
        Err(StripeError::Api(failure)) if failure.kind.as_deref() == Some("card_error") => {
            tracing::info!(company_id, stripe_code = %failure.code, "invoice payment declined");
            Ok(InvoicePayment::Declined {
                message: failure.message,
                amount_cents,
            })
        }
        Err(err) if err.is_retryable() => Err(stripe_failure(&err, company_id, "pay invoice")),
        Err(err) => {
            // A second tab paid it, or a bank debit is already in flight.
            tracing::warn!(company_id, stripe_code = err.code(), "invoice payment refused");
            let live = state.stripe.invoice(&invoice_id).await.ok();
            if live.as_ref().is_some_and(|live| live["status"] == "paid") {
                return Ok(InvoicePayment::Paid { amount_cents });
            }
            if is_processing(&record.methods, super::now()) {
                return Ok(InvoicePayment::Processing { amount_cents });
            }
            Err(ApiError::Upstream(
                "We could not take the payment. Try again.".into(),
            ))
        }
    }
}

// ---- Webhook events

/// The Stripe event types this module handles.
pub const METHOD_EVENTS: [&str; 7] = [
    "setup_intent.succeeded",
    "setup_intent.setup_failed",
    "payment_intent.processing",
    "charge.failed",
    "charge.dispute.created",
    "charge.dispute.closed",
    "payment_method.automatically_updated",
];

/// An event of [`METHOD_EVENTS`].
pub fn is_method_event(event: &Value) -> bool {
    event["type"]
        .as_str()
        .is_some_and(|kind| METHOD_EVENTS.contains(&kind))
}

/// Applies a payment method event once: a repeat delivery is a no-op, and a
/// failure releases the claim so Stripe's retry runs it again.
///
/// [`Claim::InProgress`] when another delivery is still applying it (an
/// event with nothing to apply counts as [`Claim::Done`]).
pub async fn apply_method_event(state: &AppState, event: &Value) -> ApiResult<Claim> {
    let (Some(id), Some(kind)) = (event["id"].as_str(), event["type"].as_str()) else {
        return Ok(Claim::Done);
    };
    if !state.stripe.is_enabled() {
        return Ok(Claim::Done);
    }
    let store = MySqlBillingStore(&state.pool);
    let claim = store.claim_event(id, kind).await?;
    if claim != Claim::Taken {
        return Ok(claim);
    }
    let object = &event["data"]["object"];
    let result = match kind {
        "setup_intent.succeeded" => setup_succeeded(state, id, object).await,
        "setup_intent.setup_failed" => setup_failed(state, object).await,
        "payment_intent.processing" => payment_processing(state, object).await,
        "charge.failed" => charge_failed(state, object).await,
        "charge.dispute.created" => dispute_created(state, object).await,
        "charge.dispute.closed" => dispute_closed(state, object).await,
        _ => method_updated(state, object).await,
    };
    if let Err(err) = result {
        store.release_event(id).await?;
        return Err(err);
    }
    store.finish_event(id).await?;
    Ok(claim)
}

/// A Stripe object re-read; `None` when it is gone.
fn present(read: Result<Value, StripeError>, what: &'static str) -> ApiResult<Option<Value>> {
    match read {
        Ok(object) => Ok(Some(object)),
        Err(StripeError::Missing(_)) => Ok(None),
        Err(err) => {
            tracing::warn!(stripe_code = err.code(), what, "Stripe re-read failed");
            Err(ApiError::Upstream(format!("Stripe error {}", err.code())))
        }
    }
}

/// The company of a customer, by its billing row.
async fn company_of_customer(state: &AppState, customer: Option<String>) -> ApiResult<Option<CompanyBilling>> {
    let Some(customer) = customer else {
        return Ok(None);
    };
    Ok(MySqlBillingStore(&state.pool).by_customer(&customer).await?)
}

/// The signup waiting on its bank account with exactly this SetupIntent.
async fn awaiting_with(state: &AppState, setup_intent: &Value) -> ApiResult<Option<intent::Intent>> {
    let (Some(signup_id), Some(setup_id)) = (
        setup_intent["metadata"]["signup_id"].as_str(),
        setup_intent["id"].as_str(),
    ) else {
        return Ok(None);
    };
    Ok(intent::load(&state.pool, signup_id).await?.filter(|signup| {
        signup.status == SignupStatus::AwaitingBankVerification
            && signup.setup_intent_id.as_deref() == Some(setup_id)
    }))
}

async fn setup_succeeded(state: &AppState, event_id: &str, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(setup_intent) = present(state.stripe.setup_intent(id).await, "setup intent")? else {
        return Ok(());
    };
    if setup_intent["status"] != "succeeded" {
        return Ok(());
    }
    match setup_intent["metadata"]["purpose"].as_str() {
        Some("signup") => {
            if let Some(signup) = awaiting_with(state, &setup_intent).await? {
                resume_verified(state, &signup.id, &setup_intent).await?;
            }
            Ok(())
        }
        Some(purpose @ (PURPOSE_UPDATE | PURPOSE_PAY)) => {
            let Some(company_id) = positive_int(&setup_intent["metadata"]["company_id"]) else {
                return Ok(());
            };
            let Some(record) = MySqlBillingStore(&state.pool).by_company(company_id).await? else {
                return Ok(());
            };
            // The browser applies cards and instant bank accounts itself;
            // only a stored pending bank account is the webhook's.
            if record.methods.pending_setup_intent_id.as_deref() != Some(id) {
                return Ok(());
            }
            let applied = apply_setup(state, company_id, &setup_intent).await?;
            if purpose == PURPOSE_PAY && matches!(applied, Applied::Saved { .. }) && is_overdue(&record) {
                // The method is saved; a failed payment here is not retried
                // by this event (the dunning emails continue).
                if let Err(err) = pay_open_invoice(state, company_id, &attempt_for_event(event_id)).await {
                    tracing::warn!(company_id, error = %err, "pay after bank verification failed");
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn setup_failed(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(setup_intent) = present(state.stripe.setup_intent(id).await, "setup intent")? else {
        return Ok(());
    };
    // A wrong amount keeps it `requires_action`: the Manager can try again.
    if !matches!(
        setup_intent["status"].as_str(),
        Some("requires_payment_method" | "canceled")
    ) {
        return Ok(());
    }
    if let Some(signup) = awaiting_with(state, &setup_intent).await? {
        let Some(company_id) = signup.company_id else {
            return Ok(());
        };
        let until = bank_verification_until(&state.pool, company_id)
            .await?
            .unwrap_or_else(super::now);
        email_bank_failed(state, company_id, BankFailure::Signup(until)).await;
        return Ok(());
    }
    if let Some(company_id) = positive_int(&setup_intent["metadata"]["company_id"])
        && clear_pending(state, company_id, id).await?
    {
        email_bank_failed(state, company_id, BankFailure::Replacement).await;
    }
    Ok(())
}

/// The invoice a PaymentIntent pays, re-read from Stripe.
async fn invoice_of_intent(state: &AppState, payment_intent: &str) -> ApiResult<Option<String>> {
    match state.stripe.invoice_of_payment_intent(payment_intent).await {
        Ok(invoice) => Ok(invoice),
        Err(StripeError::Missing(_)) => Ok(None),
        Err(err) => {
            tracing::warn!(
                stripe_code = err.code(),
                "Stripe invoice payment read failed"
            );
            Err(ApiError::Upstream(format!("Stripe error {}", err.code())))
        }
    }
}

/// The payment is a bank debit: the type of the method it actually uses
/// (not the types it would accept).
fn paid_by_bank(intent: &Value) -> bool {
    intent["payment_method"]["type"] == "us_bank_account"
        || intent["latest_charge"]["payment_method_details"]["type"] == "us_bank_account"
}

/// A bank payment of an invoice is on its way: marked (D18) unless that
/// invoice is paid already or its bank payment failed before. A payment of
/// no invoice lifts nothing.
async fn payment_processing(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let read = state.stripe.payment_intent_with_method(id).await;
    let Some(intent) = present(read, "payment intent")? else {
        return Ok(());
    };
    if intent["status"] != "processing" || !paid_by_bank(&intent) {
        return Ok(());
    }
    let Some(record) = company_of_customer(state, stripe_id(&intent["customer"])).await? else {
        return Ok(());
    };
    if let Some(invoice) = invoice_of_intent(state, id).await? {
        mark_processing(&state.pool, record.company_id, &invoice, super::now()).await?;
    }
    Ok(())
}

async fn charge_failed(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(charge) = present(state.stripe.charge(id).await, "charge")? else {
        return Ok(());
    };
    if charge["status"] != "failed" || charge["payment_method_details"]["type"] != "us_bank_account" {
        return Ok(());
    }
    if let Some(record) = company_of_customer(state, stripe_id(&charge["customer"])).await? {
        // Its invoice's next bank payment lifts no pause (D18, once per
        // invoice).
        if let Some(intent) = stripe_id(&charge["payment_intent"])
            && let Some(invoice) = invoice_of_intent(state, &intent).await?
        {
            record_bank_failure(&state.pool, record.company_id, &invoice, super::now()).await?;
        }
        // The bank payment in flight came back: the gate may pause again.
        clear_processing(&state.pool, record.company_id).await?;
        notify_internal(
            state,
            record.company_id,
            InternalEvent::AchReturn {
                failure_code: charge["failure_code"].as_str(),
                amount_cents: charge["amount"].as_i64(),
            },
        )
        .await;
    }
    Ok(())
}

/// A dispute row that keeps its company held: open or lost, and not
/// resolved by staff (columns of `billing_disputes`, unqualified).
pub const HOLDING_DISPUTE: &str =
    "resolved_at IS NULL AND status NOT IN ('won', 'warning_closed', 'prevented')";

/// Stripe dispute statuses that end a dispute.
fn is_closed_dispute(status: &str) -> bool {
    matches!(status, "won" | "lost" | "warning_closed" | "prevented")
}

/// One Stripe dispute, as read.
struct DisputeRecord<'a> {
    dispute_id: &'a str,
    charge_id: &'a str,
    amount_cents: Option<i64>,
    /// Stripe's status.
    status: &'a str,
    /// When Stripe opened it (`created`), else now.
    created_at: NaiveDateTime,
}

/// Records a dispute (one row per Stripe dispute). A closed one stays
/// closed: a late or retried `created` never reopens it.
async fn record_dispute(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    dispute: &DisputeRecord<'_>,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    // MySQL assigns left to right: `status` sees the old `closed_at`.
    sqlx::query(
        "INSERT INTO billing_disputes
           (company_id, stripe_dispute_id, charge_id, amount_cents, status, created_at, closed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE
           status = IF(closed_at IS NULL, VALUES(status), status),
           amount_cents = COALESCE(VALUES(amount_cents), amount_cents),
           closed_at = COALESCE(closed_at, VALUES(closed_at))",
    )
    .bind(company_id)
    .bind(dispute.dispute_id)
    .bind(dispute.charge_id)
    .bind(dispute.amount_cents)
    .bind(dispute.status)
    .bind(dispute.created_at)
    .bind(is_closed_dispute(dispute.status).then_some(now))
    .execute(pool)
    .await?;
    Ok(())
}

/// The company's dispute hold after one of its disputes changed: held while
/// any dispute holds it (whatever method was charged), lifted once none
/// does. Answers what it did and whether a hold was lifted.
async fn reconcile_dispute_hold(
    state: &AppState,
    record: &CompanyBilling,
    disputed: Disputed<'_>,
) -> ApiResult<(HoldOutcome, bool)> {
    let holding: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(COUNT(*) AS SIGNED) FROM billing_disputes
          WHERE company_id = ? AND {HOLDING_DISPUTE}"
    )))
    .bind(record.company_id)
    .fetch_one(&state.pool)
    .await?;
    if holding > 0 {
        let outcome = hold(state, record, MethodHold::Disputed, Some(disputed)).await?;
        return Ok((outcome, false));
    }
    let cleared = clear_dispute_hold(&state.pool, record.company_id).await?;
    let outcome = if is_self_serve(state, record.company_id).await? {
        HoldOutcome::Unchanged
    } else {
        HoldOutcome::Legacy
    };
    Ok((outcome, cleared))
}

/// A Stripe object's `created` time.
fn stripe_created(object: &Value) -> Option<NaiveDateTime> {
    object["created"]
        .as_i64()
        .and_then(|at| DateTime::from_timestamp(at, 0))
        .map(|at| at.naive_utc())
}

/// A new dispute: its current state is read from Stripe (a retried
/// `created` delivered after `closed` must not hold), recorded, and the
/// company held while it is open or lost, even when the charged method has
/// been replaced since. Staff are told, also when no company matches.
async fn dispute_created(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(dispute_id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(dispute) = present(state.stripe.dispute(dispute_id).await, "dispute")? else {
        return Ok(());
    };
    let Some(charge_id) = stripe_id(&dispute["charge"]) else {
        return Ok(());
    };
    let Some(charge) = present(state.stripe.charge(&charge_id).await, "charge")? else {
        return Ok(());
    };
    let amount_cents = dispute["amount"].as_i64();
    let reason = dispute["reason"].as_str();
    let customer = stripe_id(&charge["customer"]);
    let Some(record) = company_of_customer(state, customer.clone()).await? else {
        let event = InternalEvent::Dispute {
            reason,
            amount_cents,
            hold: HoldOutcome::NoCompany,
        };
        notify_unmatched(state, customer.as_deref(), event).await;
        return Ok(());
    };
    let now = super::now();
    let created_at = stripe_created(&dispute).unwrap_or(now);
    let row = DisputeRecord {
        dispute_id,
        charge_id: &charge_id,
        amount_cents,
        status: dispute["status"].as_str().unwrap_or("needs_response"),
        created_at,
    };
    record_dispute(&state.pool, record.company_id, &row, now).await?;
    let disputed = Disputed {
        charge_id: &charge_id,
        amount_cents,
        charged_at: stripe_created(&charge),
    };
    let (outcome, _) = reconcile_dispute_hold(state, &record, disputed).await?;
    let event = InternalEvent::Dispute {
        reason,
        amount_cents,
        hold: outcome,
    };
    notify_internal(state, record.company_id, event).await;
    Ok(())
}

/// A closed dispute (its outcome is final, read from the event): won (or an
/// inquiry closed without a chargeback) lifts the company's dispute hold
/// once no other dispute of it is open or lost; a lost one keeps it (and
/// holds a company the `created` event never reached) until a superuser
/// lifts it. Staff are told either way.
async fn dispute_closed(state: &AppState, object: &Value) -> ApiResult<()> {
    let (Some(dispute_id), Some(charge_id)) = (object["id"].as_str(), stripe_id(&object["charge"]))
    else {
        return Ok(());
    };
    let Some(charge) = present(state.stripe.charge(&charge_id).await, "charge")? else {
        return Ok(());
    };
    let status = object["status"].as_str();
    let amount_cents = object["amount"].as_i64();
    let customer = stripe_id(&charge["customer"]);
    let Some(record) = company_of_customer(state, customer.clone()).await? else {
        let event = InternalEvent::DisputeClosed {
            status,
            amount_cents,
            hold_cleared: false,
        };
        notify_unmatched(state, customer.as_deref(), event).await;
        return Ok(());
    };
    let now = super::now();
    let created_at = stripe_created(object).unwrap_or(now);
    let row = DisputeRecord {
        dispute_id,
        charge_id: &charge_id,
        amount_cents,
        status: status.unwrap_or("lost"),
        created_at,
    };
    record_dispute(&state.pool, record.company_id, &row, now).await?;
    let disputed = Disputed {
        charge_id: &charge_id,
        amount_cents,
        charged_at: stripe_created(&charge),
    };
    let (_, hold_cleared) = reconcile_dispute_hold(state, &record, disputed).await?;
    let event = InternalEvent::DisputeClosed {
        status,
        amount_cents,
        hold_cleared,
    };
    notify_internal(state, record.company_id, event).await;
    Ok(())
}

/// Lifts the company's dispute hold when no dispute of it holds it any
/// more; one statement, so a dispute recorded meanwhile keeps the hold.
async fn clear_dispute_hold(pool: &sqlx::MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    let cleared = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE company_billing
            SET method_hold = NULL, method_hold_at = NULL, method_hold_until = NULL,
                method_hold_charge_id = NULL, method_hold_amount_cents = NULL,
                method_hold_charged_at = NULL
          WHERE company_id = ? AND method_hold = 'disputed'
            AND NOT EXISTS (SELECT 1 FROM billing_disputes
                             WHERE billing_disputes.company_id = ? AND {HOLDING_DISPUTE})"
    )))
    .bind(company_id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(cleared.rows_affected() == 1)
}

/// A superuser lifts the company's payment method hold
/// (`POST /v1/superuser/companies/{id}/billing/payment-hold/clear`).
///
/// Its open and lost disputes are marked resolved by `user_id` (they never
/// hold it again; a new dispute does), the hold is lifted and
/// `method_hold_cleared_by`/`_at` record who did it, in one transaction.
/// Staff are told when anything changed. `true` when a hold was lifted.
pub async fn clear_hold_by_staff(
    state: &AppState,
    company_id: i32,
    user_id: i32,
) -> ApiResult<bool> {
    let now = super::now();
    let mut tx = state.pool.begin().await?;
    let resolved = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE billing_disputes SET resolved_by_user_id = ?, resolved_at = ?
          WHERE company_id = ? AND {HOLDING_DISPUTE}"
    )))
    .bind(user_id)
    .bind(now)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let previous: Option<String> = sqlx::query_scalar(
        "SELECT CAST(method_hold AS CHAR) FROM company_billing WHERE company_id = ? FOR UPDATE",
    )
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    let cleared = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE company_billing
            SET method_hold = NULL, method_hold_at = NULL, method_hold_until = NULL,
                method_hold_charge_id = NULL, method_hold_amount_cents = NULL,
                method_hold_charged_at = NULL,
                method_hold_cleared_by = ?, method_hold_cleared_at = ?
          WHERE company_id = ? AND method_hold IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM billing_disputes
                             WHERE billing_disputes.company_id = ? AND {HOLDING_DISPUTE})"
    )))
    .bind(user_id)
    .bind(now)
    .bind(company_id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    if cleared || resolved > 0 {
        let event = InternalEvent::HoldCleared {
            by_user_id: user_id,
            cleared: previous.as_deref().filter(|_| cleared),
            disputes_resolved: resolved,
        };
        notify_internal(state, company_id, event).await;
    }
    Ok(cleared)
}

async fn method_updated(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(method) = present(state.stripe.payment_method(id).await, "payment method")? else {
        return Ok(());
    };
    let blocked = &method["us_bank_account"]["status_details"]["blocked"];
    if method["type"] != "us_bank_account" || !blocked.is_object() {
        return Ok(());
    }
    let Some(record) = company_of_customer(state, stripe_id(&method["customer"])).await? else {
        return Ok(());
    };
    if record.stripe_payment_method_id.as_deref() != Some(id) {
        return Ok(());
    }
    let outcome = hold(state, &record, MethodHold::Blocked, None).await?;
    notify_internal(
        state,
        record.company_id,
        InternalEvent::Blocked {
            reason: blocked["reason"].as_str(),
            hold: outcome,
        },
    )
    .await;
    Ok(())
}

/// The disputed charge behind a dispute hold.
#[derive(Debug, Clone, Copy)]
pub struct Disputed<'a> {
    pub charge_id: &'a str,
    pub amount_cents: Option<i64>,
    /// When the disputed payment was made.
    pub charged_at: Option<NaiveDateTime>,
}

/// The company is on a self-serve plan (holds apply to it).
async fn is_self_serve(state: &AppState, company_id: i32) -> sqlx::Result<bool> {
    let self_serve: Option<bool> = sqlx::query_scalar(
        "SELECT COALESCE(plan_source = 'subscription', 0) FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(self_serve == Some(true))
}

/// Sets a hold on a self-serve company unless one runs already (its end
/// never moves), then tries its email at once (the billing reminders job
/// sends what is left).
async fn hold(
    state: &AppState,
    record: &CompanyBilling,
    kind: MethodHold,
    disputed: Option<Disputed<'_>>,
) -> ApiResult<HoldOutcome> {
    if !is_self_serve(state, record.company_id).await? {
        return Ok(HoldOutcome::Legacy);
    }
    let read = record.stripe_payment_method_id.as_deref();
    if !set_hold(&state.pool, record.company_id, read, kind, disputed, super::now()).await? {
        return Ok(HoldOutcome::Unchanged);
    }
    send_hold_emails(state, record.company_id).await;
    Ok(HoldOutcome::Set)
}

/// Writes a hold set at `now` unless one runs already; `true` when it did
/// (its email is then due).
///
/// A blocked-account hold lands only while the company's saved method is
/// still `read_method` (the one blocked, as read before): a Manager who
/// saved a new method meanwhile (which clears that hold) is not held. A
/// dispute holds whatever method is saved now, and turns a running
/// blocked-account hold into a dispute hold: a new method must not lift it.
/// The converted hold keeps its end (the Managers were already told that
/// date) and starts a new email cycle at `now`, so they learn that a new
/// method no longer lifts it.
pub async fn set_hold(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    read_method: Option<&str>,
    kind: MethodHold,
    disputed: Option<Disputed<'_>>,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let set = sqlx::query(
        "UPDATE company_billing
            SET method_hold = ?, method_hold_at = ?, method_hold_until = ?,
                method_hold_charge_id = ?, method_hold_amount_cents = ?,
                method_hold_charged_at = ?
          WHERE company_id = ? AND method_hold IS NULL
            AND (? = 'disputed' OR stripe_payment_method_id <=> ?)",
    )
    .bind(kind.as_str())
    .bind(now)
    .bind(hold_until(now))
    .bind(disputed.map(|disputed| disputed.charge_id))
    .bind(disputed.and_then(|disputed| disputed.amount_cents))
    .bind(disputed.and_then(|disputed| disputed.charged_at))
    .bind(company_id)
    .bind(kind.as_str())
    .bind(read_method)
    .execute(pool)
    .await?;
    if set.rows_affected() == 1 {
        return Ok(true);
    }
    if kind == MethodHold::Disputed {
        let converted = sqlx::query(
            "UPDATE company_billing
                SET method_hold = 'disputed', method_hold_at = ?, method_hold_charge_id = ?,
                    method_hold_amount_cents = ?, method_hold_charged_at = ?
              WHERE company_id = ? AND method_hold = 'blocked'",
        )
        .bind(now)
        .bind(disputed.map(|disputed| disputed.charge_id))
        .bind(disputed.and_then(|disputed| disputed.amount_cents))
        .bind(disputed.and_then(|disputed| disputed.charged_at))
        .bind(company_id)
        .execute(pool)
        .await?;
        return Ok(converted.rows_affected() == 1);
    }
    Ok(false)
}

/// Sends the hold email to the Managers who do not have it yet, each claimed
/// in `billing_reminder_emails` (`method_hold`) like the dunning emails, so
/// the billing reminders job retries one that failed (refused or not
/// answered: a second copy beats none) and never sends a delivered one
/// again. Failures are logged; the job carries on.
async fn send_hold_emails(state: &AppState, company_id: i32) {
    let store = MySqlReminderStore(&state.pool);
    let now = super::now();
    let plan = match hold_plan(&store, company_id, now).await {
        Ok(Some(plan)) => plan,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(company_id, error = %err, "payment hold email not planned");
            return;
        }
    };
    let company = company_name(&state.pool, company_id).await;
    let account_url = payments_account_url();
    for contact in &plan.contacts {
        let key = ReminderKey {
            company_id,
            user_id: contact.user_id,
            cycle: plan.due.cycle,
            step: plan.due.step,
        };
        let id = match store.claim(&key, now).await {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(company_id, error = %err, "payment hold email not claimed");
                continue;
            }
        };
        let claimed = ClaimedReminder { id, company_id };
        let email = render_hold_email(&plan.facts, &company, contact, &account_url);
        let rendered = super::reminder_emails::RenderedEmail {
            subject: email.subject,
            html: email.html,
            text: email.text,
        };
        let recorded = match send_rendered(state, &contact.email, &rendered).await {
            Ok(()) => store.mark_sent(claimed, None, now).await,
            Err(err) => {
                tracing::warn!(company_id, kind = failure_kind(&err), "payment hold email failed");
                match classify_send_failure(&err) {
                    ReminderSendResult::Rejected { code } | ReminderSendResult::Unknown { code } => {
                        store.mark_failed(claimed, &code).await
                    }
                    ReminderSendResult::Sent { .. } => Ok(()),
                }
            }
        };
        if let Err(err) = recorded {
            tracing::warn!(company_id, error = %err, "payment hold email outcome not recorded");
        }
    }
}

async fn email_bank_failed(state: &AppState, company_id: i32, failure: BankFailure) {
    let company = company_name(&state.pool, company_id).await;
    let account_url = payments_account_url();
    for contact in managers(state, company_id).await {
        let email = render_bank_failed(&BankFailedInput {
            failure,
            company_name: &company,
            user_name: contact.name.as_deref(),
            account_url: &account_url,
        });
        if let Err(err) = send_rendered(state, &contact.email, &email).await {
            tracing::warn!(company_id, kind = failure_kind(&err), "bank verification email failed");
        }
    }
}

async fn managers(state: &AppState, company_id: i32) -> Vec<super::cancellation::ManagerContact> {
    manager_contacts(&state.pool, company_id)
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(company_id, "payment method email recipients not read");
            Vec::new()
        })
}

/// Sends one rendered payment method email from `billing::from_address`,
/// replies going to support.
async fn send_rendered(
    state: &AppState,
    to: &str,
    email: &super::reminder_emails::RenderedEmail,
) -> Result<(), crate::webhooks_client::SendFailure> {
    let message = crate::webhooks_client::OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: super::from_address(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: email.subject.clone(),
        body: email.html.clone(),
        signature: None,
        reply_to: vec![super::reminder_emails::SUPPORT_EMAIL.into()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    state
        .webhooks
        .send_notice(&message, &email.text, EMAIL_TIMEOUT)
        .await
        .map(|_| ())
}

/// The internal notice, when `BILLING_ALERT_EMAIL` is set.
async fn notify_internal(state: &AppState, company_id: i32, event: InternalEvent<'_>) {
    if alert_email(state).is_none() {
        return;
    }
    let company = company_name(&state.pool, company_id).await;
    send_internal(
        state,
        &InternalNotice {
            event,
            company_id: Some(company_id),
            company_name: &company,
            stripe_customer: None,
        },
    )
    .await;
}

/// The internal notice for an event of a Stripe customer no company has.
async fn notify_unmatched(state: &AppState, customer: Option<&str>, event: InternalEvent<'_>) {
    send_internal(
        state,
        &InternalNotice {
            event,
            company_id: None,
            company_name: "",
            stripe_customer: customer,
        },
    )
    .await;
}

async fn send_internal(state: &AppState, notice: &InternalNotice<'_>) {
    let Some(to) = alert_email(state) else {
        return;
    };
    let company_id = notice.company_id;
    let email = render_internal_notice(notice);
    if let Err(err) = send_rendered(state, to, &email).await {
        // Never retried (the event is claimed): staff hear of it through
        // Error Tracking instead.
        tracing::error!(company_id, kind = failure_kind(&err), "billing alert email failed");
        let properties = serde_json::json!({
            "$exception_list": [{
                "type": "BillingAlertEmailError",
                "value": "billing alert email failed, not resent",
                "mechanism": { "handled": true, "synthetic": false },
            }],
            "$process_person_profile": false,
            "error_source": "api",
            "company_id": company_id,
            "error_code": failure_kind(&err),
        });
        crate::posthog::capture(&POSTHOG_HTTP, "$exception", "granite-api", properties).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;
    use serde_json::json;

    #[test]
    fn policies_are_named_numbers() {
        assert_eq!(HOLD_DAYS, 7);
        assert_eq!(PROCESSING_DAYS, 10);
        assert_eq!(hold_until(at("2026-11-01 10:00:00")), at("2026-11-08 10:00:00"));
        assert!(RETRY_POLICY.contains("up to 2 retries within 40 days"));
        assert!(!RETRY_POLICY.contains('\u{2014}'));
    }

    #[test]
    fn processing_protects_for_ten_days_only() {
        let methods = MethodState {
            payment_processing_at: Some(at("2026-11-01 00:00:00")),
            ..MethodState::default()
        };
        assert!(is_processing(&methods, at("2026-11-10 23:59:59")));
        assert!(!is_processing(&methods, at("2026-11-11 00:00:00")));
        assert!(!is_processing(&MethodState::default(), at("2026-11-01 00:00:00")));
    }

    #[test]
    fn a_hold_ends_at_its_until() {
        let methods = MethodState {
            hold: Some(MethodHold::Blocked),
            hold_at: Some(at("2026-11-01 00:00:00")),
            hold_until: Some(at("2026-11-08 00:00:00")),
            ..MethodState::default()
        };
        assert!(!hold_reached_end(&methods, at("2026-11-07 23:59:59")));
        assert!(hold_reached_end(&methods, at("2026-11-08 00:00:00")));
        assert!(!hold_reached_end(&MethodState::default(), at("2030-01-01 00:00:00")));
    }

    #[test]
    fn microdeposits_are_read_from_the_next_action() {
        let pending = json!({
            "id": "seti_1", "status": "requires_action", "created": 1_800_000_000,
            "next_action": { "type": "verify_with_microdeposits", "verify_with_microdeposits": {
                "hosted_verification_url": "https://payments.stripe.com/microdeposit/x" } },
            "payment_method": { "type": "us_bank_account", "us_bank_account": { "last4": "6789", "bank_name": "STRIPE TEST BANK" } },
        });
        assert!(is_microdeposits(&pending));
        assert_eq!(
            verify_url(&pending).as_deref(),
            Some("https://payments.stripe.com/microdeposit/x")
        );
        assert_eq!(
            verify_by(&pending),
            DateTime::from_timestamp(1_800_000_000 + 10 * 86_400, 0).map(|d| d.naive_utc())
        );
        assert_eq!(
            method_type_and_last4(&pending["payment_method"]),
            (Some("us_bank_account".into()), Some("6789".into()))
        );
        assert_eq!(method_type_and_last4(&json!("pm_1")), (None, None));
        assert!(!is_microdeposits(&json!({ "status": "requires_action",
            "next_action": { "type": "use_stripe_sdk" } })));
    }

    #[test]
    fn an_event_always_pays_under_the_same_uuid() {
        let attempt = attempt_for_event("evt_123");
        assert_eq!(attempt, attempt_for_event("evt_123"));
        assert_ne!(attempt, attempt_for_event("evt_124"));
        assert!(uuid::Uuid::parse_str(&attempt).is_ok());
    }

    #[test]
    fn the_oldest_open_subscription_invoice_is_paid() {
        let invoices = [
            json!({ "id": "in_new", "status": "open", "created": 300, "amount_remaining": 100 }),
            json!({ "id": "in_old", "status": "open", "created": 200, "amount_remaining": 100 }),
            json!({ "id": "in_zero", "status": "open", "created": 50, "amount_remaining": 0 }),
            json!({ "id": "in_paid", "status": "paid", "created": 10, "amount_remaining": 0 }),
        ];
        let invoices = invoices.map(|mut invoice| {
            invoice["parent"] = json!({ "subscription_details": { "subscription": "sub_1" } });
            invoice
        });
        assert_eq!(
            oldest_open(&invoices, Some("sub_1")).unwrap()["id"],
            "in_old"
        );
        assert!(oldest_open(&invoices[2..], Some("sub_1")).is_none());
        assert!(oldest_open(&invoices, None).is_none());
        // An older invoice of another (or no) subscription is never paid.
        let legacy = json!({ "id": "in_legacy", "status": "open", "created": 1,
            "amount_remaining": 100, "subscription": "sub_old" });
        let by_hand = json!({ "id": "in_hand", "status": "open", "created": 2,
            "amount_remaining": 100 });
        let mixed = [legacy.clone(), by_hand, invoices[0].clone()];
        assert_eq!(oldest_open(&mixed, Some("sub_1")).unwrap()["id"], "in_new");
        assert_eq!(
            oldest_open(&[legacy], Some("sub_old")).unwrap()["id"],
            "in_legacy"
        );
    }

    #[test]
    fn only_the_method_events_come_here() {
        assert!(is_method_event(&json!({ "type": "setup_intent.succeeded" })));
        assert!(is_method_event(&json!({ "type": "charge.dispute.created" })));
        assert!(is_method_event(&json!({ "type": "charge.dispute.closed" })));
        assert!(!is_method_event(&json!({ "type": "invoice.paid" })));
        assert!(!is_method_event(&json!({ "type": "payment_method.updated" })));
    }

    #[test]
    fn a_setup_intent_must_name_the_company_and_its_customer() {
        let record = CompanyBilling {
            stripe_customer_id: Some("cus_1".into()),
            ..CompanyBilling::empty(7)
        };
        let si = |company: &str, customer: &str| {
            json!({ "metadata": { "company_id": company }, "customer": customer })
        };
        assert!(belongs_to(Some(&record), 7, &si("7", "cus_1")));
        assert!(!belongs_to(Some(&record), 7, &si("8", "cus_1")));
        assert!(!belongs_to(Some(&record), 7, &si("7", "cus_2")));
        assert!(!belongs_to(None, 7, &si("7", "cus_1")));
    }
}
