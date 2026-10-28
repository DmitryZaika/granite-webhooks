//! Card and bank account payment methods (self-serve billing step 13).
//!
//! One saved method per company (D5): a Manager's new method becomes the
//! customer's and the subscription's default, then the old one is detached.
//! A bank account waiting for micro-deposits is kept as
//! `company_billing.pending_setup_intent_id` until Stripe verifies it. Holds
//! (a blocked bank account, a dispute) give the company [`HOLD_DAYS`] to save
//! a new method. Paying an open invoice never changes `payment_status`
//! (D17): only `invoice.paid` or the gate's Stripe read do.
//!
//! The Stripe events of this module (`setup_intent.*`,
//! `payment_intent.processing`, `charge.failed`, `charge.dispute.created`,
//! `payment_method.automatically_updated`) are claimed once by event id and
//! re-read from Stripe before anything changes, so a late or out-of-order
//! event acts on Stripe's current state. Logs carry Stripe error codes only.

use super::addons::{AccountPaymentMethod, payment_method};
use super::cancellation::{company_name, failure_kind, manager_contacts, send_rendered};
use super::lock::{CompanyLock, LockError};
use super::method_emails::{
    BankFailedInput, BankFailure, HoldInput, InternalEvent, InternalNotice, render_bank_failed,
    render_hold, render_internal_notice,
};
use super::store::{BillingStore, MySqlBillingStore, bank_verification_until};
use super::stripe::{SetupPurpose, StripeError, positive_int, stripe_id};
use super::{
    CompanyBilling, MethodHold, MethodState, is_ended_subscription_status, is_overdue,
    payments_account_url,
};
use crate::error::{ApiError, ApiResult};
use crate::onboarding_call::fee::is_fee_invoice_object;
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

/// Days Stripe leaves micro-deposits to be verified before the SetupIntent
/// times out.
pub const MICRODEPOSIT_DAYS: i64 = 10;

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

/// When the micro-deposits must be verified: created + [`MICRODEPOSIT_DAYS`].
pub fn verify_by(setup_intent: &Value) -> Option<NaiveDateTime> {
    let created = DateTime::from_timestamp(setup_intent["created"].as_i64()?, 0)?;
    Some(created.naive_utc() + Duration::days(MICRODEPOSIT_DAYS))
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
/// one is detached, holds and the pending bank account are cleared.
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
    sqlx::query(
        "UPDATE company_billing
            SET stripe_payment_method_id = ?, pending_setup_intent_id = NULL,
                method_hold = NULL, method_hold_at = NULL, method_hold_until = NULL
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

/// The oldest open subscription invoice with something left to pay (never
/// the onboarding fee).
fn oldest_open(invoices: &[Value]) -> Option<&Value> {
    invoices
        .iter()
        .filter(|invoice| invoice["status"] == "open")
        .filter(|invoice| !is_fee_invoice_object(invoice))
        .filter(|invoice| amount_of(invoice) > 0)
        .min_by_key(|invoice| {
            (
                invoice["created"].as_i64().unwrap_or(i64::MAX),
                invoice["id"].as_str().unwrap_or_default().to_string(),
            )
        })
}

/// Marks a bank payment in flight; an older mark past [`PROCESSING_DAYS`]
/// starts over.
pub async fn mark_processing(
    pool: &sqlx::MySqlPool,
    company_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE company_billing
            SET payment_processing_at = IF(payment_processing_at IS NULL
                                             OR payment_processing_at <= ?, ?, payment_processing_at)
          WHERE company_id = ?",
    )
    .bind(now - Duration::days(PROCESSING_DAYS))
    .bind(now)
    .bind(company_id)
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
/// `payment_processing_at`. 400 when `attempt_id` is not a UUID, 409
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
    let Some(invoice) = oldest_open(&invoices) else {
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
            mark_processing(&state.pool, company_id, super::now()).await?;
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
pub const METHOD_EVENTS: [&str; 6] = [
    "setup_intent.succeeded",
    "setup_intent.setup_failed",
    "payment_intent.processing",
    "charge.failed",
    "charge.dispute.created",
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
pub async fn apply_method_event(state: &AppState, event: &Value) -> ApiResult<()> {
    let (Some(id), Some(kind)) = (event["id"].as_str(), event["type"].as_str()) else {
        return Ok(());
    };
    if !state.stripe.is_enabled() {
        return Ok(());
    }
    let store = MySqlBillingStore(&state.pool);
    if !store.claim_event(id, kind).await? {
        return Ok(());
    }
    let object = &event["data"]["object"];
    let result = match kind {
        "setup_intent.succeeded" => setup_succeeded(state, id, object).await,
        "setup_intent.setup_failed" => setup_failed(state, object).await,
        "payment_intent.processing" => payment_processing(state, object).await,
        "charge.failed" => charge_failed(state, object).await,
        "charge.dispute.created" => dispute_created(state, object).await,
        _ => method_updated(state, object).await,
    };
    if let Err(err) = result {
        store.release_event(id).await?;
        return Err(err);
    }
    Ok(())
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

async fn payment_processing(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(id) = object["id"].as_str() else {
        return Ok(());
    };
    let Some(intent) = present(state.stripe.payment_intent(id).await, "payment intent")? else {
        return Ok(());
    };
    let bank = intent["payment_method_types"]
        .as_array()
        .is_some_and(|types| types.iter().any(|kind| kind == "us_bank_account"));
    if intent["status"] != "processing" || !bank {
        return Ok(());
    }
    if let Some(record) = company_of_customer(state, stripe_id(&intent["customer"])).await? {
        mark_processing(&state.pool, record.company_id, super::now()).await?;
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

async fn dispute_created(state: &AppState, object: &Value) -> ApiResult<()> {
    let Some(charge_id) = stripe_id(&object["charge"]) else {
        return Ok(());
    };
    let Some(charge) = present(state.stripe.charge(&charge_id).await, "charge")? else {
        return Ok(());
    };
    let Some(record) = company_of_customer(state, stripe_id(&charge["customer"])).await? else {
        return Ok(());
    };
    // A dispute on a method the company has since replaced holds nothing.
    let charged = stripe_id(&charge["payment_method"]);
    if record.stripe_payment_method_id.is_none() || charged == record.stripe_payment_method_id {
        hold(state, &record, MethodHold::Disputed).await?;
    }
    notify_internal(
        state,
        record.company_id,
        InternalEvent::Dispute {
            reason: object["reason"].as_str(),
            amount_cents: object["amount"].as_i64(),
        },
    )
    .await;
    Ok(())
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
    hold(state, &record, MethodHold::Blocked).await?;
    notify_internal(
        state,
        record.company_id,
        InternalEvent::Blocked {
            reason: blocked["reason"].as_str(),
        },
    )
    .await;
    Ok(())
}

/// Sets a hold unless one runs already (its end never moves), and emails
/// the Managers when it is new.
async fn hold(state: &AppState, record: &CompanyBilling, kind: MethodHold) -> ApiResult<()> {
    let now = super::now();
    let until = hold_until(now);
    let set = sqlx::query(
        "UPDATE company_billing
            SET method_hold = ?, method_hold_at = ?, method_hold_until = ?
          WHERE company_id = ? AND method_hold IS NULL",
    )
    .bind(kind.as_str())
    .bind(now)
    .bind(until)
    .bind(record.company_id)
    .execute(&state.pool)
    .await?;
    if set.rows_affected() == 1 {
        email_hold(state, record.company_id, kind, until).await;
    }
    Ok(())
}

async fn email_hold(state: &AppState, company_id: i32, kind: MethodHold, until: NaiveDateTime) {
    let company = company_name(&state.pool, company_id).await;
    let account_url = payments_account_url();
    for contact in managers(state, company_id).await {
        let email = render_hold(&HoldInput {
            hold: kind,
            company_name: &company,
            user_name: contact.name.as_deref(),
            pauses_at: until,
            account_url: &account_url,
        });
        if let Err(err) = send_rendered(state, &contact.email, &email).await {
            tracing::warn!(company_id, kind = failure_kind(&err), "payment hold email failed");
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

/// The internal notice, when `BILLING_ALERT_EMAIL` is set.
async fn notify_internal(state: &AppState, company_id: i32, event: InternalEvent<'_>) {
    let Some(to) = alert_email(state) else {
        return;
    };
    let company = company_name(&state.pool, company_id).await;
    let email = render_internal_notice(&InternalNotice {
        event,
        company_id,
        company_name: &company,
    });
    if let Err(err) = send_rendered(state, to, &email).await {
        tracing::warn!(company_id, kind = failure_kind(&err), "billing alert email failed");
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
    fn the_oldest_open_subscription_invoice_is_paid_never_the_fee() {
        let invoices = [
            json!({ "id": "in_new", "status": "open", "created": 300, "amount_remaining": 100 }),
            json!({ "id": "in_fee", "status": "open", "created": 100, "amount_remaining": 9_900,
                "metadata": { "kind": "onboarding_fee" } }),
            json!({ "id": "in_old", "status": "open", "created": 200, "amount_remaining": 100 }),
            json!({ "id": "in_zero", "status": "open", "created": 50, "amount_remaining": 0 }),
            json!({ "id": "in_paid", "status": "paid", "created": 10, "amount_remaining": 0 }),
        ];
        assert_eq!(oldest_open(&invoices).unwrap()["id"], "in_old");
        assert!(oldest_open(&invoices[1..2]).is_none());
    }

    #[test]
    fn only_the_six_new_events_come_here() {
        assert!(is_method_event(&json!({ "type": "setup_intent.succeeded" })));
        assert!(is_method_event(&json!({ "type": "charge.dispute.created" })));
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
