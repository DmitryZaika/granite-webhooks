//! The onboarding fee in Stripe (03-stripe.md section 3.5) and its webhook
//! events.
//!
//! The invoice is created first and the fee attached to it with `invoice=`,
//! so no other pending item of the customer is swept in. A run that stopped
//! halfway finds its invoice again by `metadata[kind]=onboarding_fee` and
//! `metadata[deadline]`, and the commitment records the invoice before it is
//! finalized, so it is never charged twice. Nothing reaches Stripe unless
//! charging is on, the consent is on file, the subscription runs, no
//! cancellation was requested before the charge date and that date has come
//! ([`hold_reason`], [`withdrawal`]); all of it is read again right before
//! the invoice is finalized and right before it is paid. A recorded invoice
//! that may not be charged is withdrawn: a draft deleted, an open one never
//! attempted voided ([`invoice_withdrawn`]). Fee invoice events never reach
//! subscription billing.
//!
//! The invoice is created with `auto_advance=false`, so Stripe never retries
//! a declined payment: `fee_failed` stays until staff follow up by hand (a
//! payment ends it `fee_paid`, a void `waived`), and staff are told when it
//! fails. Any other Stripe failure is retried by the next daily run.

use super::commitment::{
    ChargeFacts, Commitment, EventKind, InvoiceEvent, Outcome, Status, Withdrawal, invoice_created,
    invoice_event, invoice_withdrawn, withdrawal,
};
use super::store::{self, SaveError, Stored};
use crate::billing;
use crate::billing::catalog::SIGNUP_TZ;
use crate::billing::reminder_emails::{escape_html, format_money};
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::stripe::{FeeInvoice, StripeError, positive_int};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use chrono::{NaiveDateTime, TimeZone};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;

const DEFAULT_FROM_ADDRESS: &str = "noreply@granite-manager.com";

/// `metadata[kind]` of the fee invoice.
pub const FEE_KIND: &str = "onboarding_fee";

#[derive(Debug, thiserror::Error)]
pub enum FeeError {
    /// No Stripe customer to invoice: reported, tried again the next day.
    #[error("the company has no Stripe customer")]
    NoCustomer,
    /// Stripe refused or failed (not a card decline); the code only. Tried
    /// again the next day.
    #[error("Stripe failed: {0}")]
    Stripe(String),
    /// The fee may not be charged now ([`hold_reason`], [`withdrawal`]):
    /// nothing was sent to Stripe. Reported, and looked at again the next day.
    #[error("the onboarding fee is held: {0}")]
    Held(&'static str),
    #[error(transparent)]
    Save(#[from] SaveError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl FeeError {
    /// Safe to log and report.
    pub fn code(&self) -> String {
        match self {
            Self::NoCustomer => "no_stripe_customer".into(),
            Self::Stripe(code) => code.clone(),
            Self::Held(reason) => format!("held_{reason}"),
            Self::Save(SaveError::Conflict) => "conflict".into(),
            Self::Save(SaveError::Database(_)) | Self::Database(_) => "database".into(),
        }
    }
}

fn stripe_error(err: &StripeError) -> FeeError {
    FeeError::Stripe(err.code().to_string())
}

/// A card decline: HTTP 402 or Stripe's `card_error`. Only this is
/// `fee_failed`; any other failure is retried.
fn is_decline(err: &StripeError) -> bool {
    matches!(err, StripeError::Api(failure)
        if failure.status == 402 || failure.kind.as_deref() == Some("card_error"))
}

/// Where the fee stands after a charge attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charged {
    Paid,
    /// Declined: `fee_failed`. Stripe does not retry it (`auto_advance=false`):
    /// staff are told and follow up by hand.
    Failed,
    /// The payment is on its way (a bank debit), or Stripe was not asked
    /// again: stays `fee_invoiced`.
    Processing,
    /// The invoice was deleted or voided before any charge
    /// ([`invoice_withdrawn`]): nothing is owed.
    Withdrawn,
}

/// `YYYY-MM-DD` of the deadline, its day in `SIGNUP_TZ` (the day the
/// consent named): `metadata[deadline]` and the creation idempotency key.
pub fn deadline_key(deadline_at: NaiveDateTime) -> String {
    SIGNUP_TZ
        .from_utc_datetime(&deadline_at)
        .format("%Y-%m-%d")
        .to_string()
}

/// The idempotency key of the fee invoice's creation; after `recorded`
/// withdrawn invoices it carries their count, so Stripe creates a new one.
fn create_key(company_id: i32, deadline: &str, recorded: i64) -> String {
    if recorded > 0 {
        format!("onboarding-fee:{company_id}:{deadline}:{recorded}")
    } else {
        format!("onboarding-fee:{company_id}:{deadline}")
    }
}

/// `From:` of the staff alert: `BILLING_EMAIL_FROM_ADDRESS`, as the other
/// onboarding emails.
pub(super) fn from_address() -> String {
    std::env::var("BILLING_EMAIL_FROM_ADDRESS")
        .ok()
        .map(|address| address.trim().to_string())
        .filter(|address| !address.is_empty())
        .unwrap_or_else(|| DEFAULT_FROM_ADDRESS.to_string())
}

fn is_fee_invoice(invoice: &Value, company_id: i32, deadline: &str) -> bool {
    let metadata = &invoice["metadata"];
    metadata["kind"] == FEE_KIND
        && metadata["deadline"] == deadline
        && positive_int(&metadata["company_id"]) == Some(company_id)
        && invoice["status"] != "void"
}

fn has_lines(invoice: &Value) -> bool {
    invoice["lines"]["data"]
        .as_array()
        .is_some_and(|lines| !lines.is_empty())
}

/// Why the fee may not be charged now, or `None` when it may: the status is
/// `notice_sent` or `fee_invoiced`, charging is on, the `onboarding_fee`
/// consent is on file, the notice went out and its charge date has come.
pub fn hold_reason(
    commitment: &Commitment,
    charging: bool,
    consent: bool,
    now: NaiveDateTime,
) -> Option<&'static str> {
    if !matches!(commitment.status, Status::NoticeSent | Status::FeeInvoiced) {
        Some("status")
    } else if !charging {
        Some("charging_off")
    } else if !consent {
        Some("no_consent")
    } else if commitment.notice_sent_at.is_none() {
        Some("no_notice")
    } else if commitment.charge_after.is_none_or(|at| at > now) {
        Some("before_charge_date")
    } else {
        None
    }
}

async fn charge_facts(state: &AppState, company_id: i32) -> Result<ChargeFacts, FeeError> {
    let pool = &state.pool;
    Ok(ChargeFacts {
        charging_enabled: state.onboarding_call.fee_charging,
        consent_on_file: store::consent_on_file(pool, company_id).await?,
        subscription_ended: store::subscription_ended(pool, company_id).await?,
        cancellation_requested_at: store::cancellation_requested_at(pool, company_id).await?,
    })
}

/// Applies `change` to the commitment as stored (read again on a conflict);
/// the outcome saved, `None` when it changed nothing.
async fn apply(
    pool: &MySqlPool,
    company_id: i32,
    now: NaiveDateTime,
    change: impl Fn(&Commitment) -> Outcome,
) -> Result<Option<Outcome>, FeeError> {
    for _ in 0..2 {
        let Some(stored) = store::load(pool, company_id).await? else {
            return Ok(None);
        };
        let outcome = change(&stored.commitment);
        if !store::changes(&stored, &outcome) {
            return Ok(None);
        }
        match store::save(pool, stored.version, &outcome, now).await {
            Ok(_) => return Ok(Some(outcome)),
            Err(SaveError::Conflict) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(SaveError::Conflict.into())
}

/// Records a fee invoice event on the commitment.
async fn record_event(
    pool: &MySqlPool,
    company_id: i32,
    invoice_id: &str,
    event: InvoiceEvent,
    now: NaiveDateTime,
) -> Result<Option<Outcome>, FeeError> {
    apply(pool, company_id, now, |c| {
        invoice_event(c, invoice_id, event, now)
    })
    .await
}

/// Withdraws the recorded fee invoice `invoice_id` before any charge: a
/// draft is deleted, an open invoice never attempted voided, then the
/// commitment moves on ([`invoice_withdrawn`]). A paid invoice is recorded
/// as paid; one with a payment under way is left alone ([`FeeError::Held`]).
async fn withdraw(
    state: &AppState,
    company_id: i32,
    invoice_id: &str,
    reason: Withdrawal,
    now: NaiveDateTime,
) -> Result<Charged, FeeError> {
    let stripe = &state.stripe;
    let invoice = match stripe.invoice(invoice_id).await {
        Ok(invoice) => Some(invoice),
        Err(StripeError::Missing(_)) => None,
        Err(err) => return Err(stripe_error(&err)),
    };
    let state_of = invoice
        .as_ref()
        .map(|invoice| (invoice["status"].as_str(), invoice["attempted"] == true));
    match state_of {
        None | Some((Some("void"), _)) => {}
        Some((Some("draft"), _)) => match stripe.delete_invoice(invoice_id).await {
            Ok(_) | Err(StripeError::Missing(_)) => {}
            Err(err) => return Err(stripe_error(&err)),
        },
        Some((Some("open"), false)) => {
            stripe
                .void_invoice(invoice_id, &format!("onboarding-fee:{invoice_id}:void"))
                .await
                .map_err(|e| stripe_error(&e))?;
        }
        Some((Some("paid"), _)) => {
            record_event(&state.pool, company_id, invoice_id, InvoiceEvent::Paid, now).await?;
            return Ok(Charged::Paid);
        }
        Some(_) => return Err(FeeError::Held(reason.as_str())),
    }
    apply(&state.pool, company_id, now, |c| {
        if c.stripe_invoice_id.as_deref() == Some(invoice_id) {
            invoice_withdrawn(c, reason, now)
        } else {
            Outcome {
                commitment: c.clone(),
                events: Vec::new(),
            }
        }
    })
    .await?;
    tracing::info!(
        company_id,
        reason = reason.as_str(),
        "onboarding fee invoice withdrawn"
    );
    Ok(Charged::Withdrawn)
}

/// What the run may do next with the recorded invoice `invoice_id`.
enum Recheck {
    Go,
    Withdraw(Withdrawal),
    Stop(&'static str),
}

/// Reads the commitment and the facts again right before Stripe finalizes
/// or charges: the invoice must still be the recorded `fee_invoiced` one and
/// the fee chargeable.
async fn recheck(
    state: &AppState,
    company_id: i32,
    invoice_id: &str,
    now: NaiveDateTime,
) -> Result<Recheck, FeeError> {
    let Some(stored) = store::load(&state.pool, company_id).await? else {
        return Ok(Recheck::Stop("status"));
    };
    let c = &stored.commitment;
    if c.status != Status::FeeInvoiced || c.stripe_invoice_id.as_deref() != Some(invoice_id) {
        return Ok(Recheck::Stop("status"));
    }
    let facts = charge_facts(state, company_id).await?;
    if let Some(reason) = withdrawal(c, &facts) {
        return Ok(Recheck::Withdraw(reason));
    }
    Ok(
        hold_reason(c, facts.charging_enabled, facts.consent_on_file, now)
            .map_or(Recheck::Go, Recheck::Stop),
    )
}

/// [`recheck`]: `Some` result when the run stops here (withdrawn or held).
async fn stop_unless_chargeable(
    state: &AppState,
    company_id: i32,
    invoice_id: &str,
    now: NaiveDateTime,
) -> Result<Option<Charged>, FeeError> {
    match recheck(state, company_id, invoice_id, now).await? {
        Recheck::Go => Ok(None),
        Recheck::Withdraw(reason) => withdraw(state, company_id, invoice_id, reason, now)
            .await
            .map(Some),
        Recheck::Stop(reason) => Err(FeeError::Held(reason)),
    }
}

/// The failure without its text, which can quote the recipient's address.
const fn failure_kind(err: &SendFailure) -> &'static str {
    match err {
        SendFailure::Rejected(_) => "rejected",
        SendFailure::Failed(_) => "failed",
    }
}

/// Tells staff the fee payment failed, for a follow-up by hand: Telegram to
/// `SIGNUP_NOTIFY_USER_ID`, email to `SIGNUP_NOTIFY_EMAIL`, with the company
/// id, name and amount only. Best effort: a failure is logged and dropped.
async fn notify_failed(state: &AppState, commitment: &Commitment) {
    let company_id = commitment.company_id;
    let name = store::company_name(&state.pool, company_id)
        .await
        .unwrap_or_default();
    let amount = format_money(commitment.fee_cents);
    if let Some(staff) = state.signup.notify_user_id {
        let text = format!(
            "Onboarding fee payment failed\nCompany: {name} (id {company_id})\nAmount: {amount}\nStripe does not retry it: follow up by hand."
        );
        if !state.webhooks.notify_telegram(staff, &text).await {
            tracing::warn!(company_id, "onboarding fee failure Telegram notice failed");
        }
    }
    if let Some(to) = state.signup.notify_email.as_deref() {
        let body = format!(
            r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Onboarding fee payment failed</h2>
  <p style="color: #475569; line-height: 1.6;">Company: {} (id {company_id})<br>Amount: {amount}</p>
  <p style="color: #475569; line-height: 1.6;">Stripe does not retry this invoice. Follow up with the company by hand: a payment marks the fee paid, voiding the invoice waives it.</p>
</div>"#,
            escape_html(&name)
        );
        let email = OutgoingEmail {
            from_name: Some("Granite Manager".into()),
            from_address: from_address(),
            to: vec![to.to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: format!("Onboarding fee payment failed: {name}"),
            body,
            signature: None,
            reply_to: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            transactional: true,
        };
        if let Err(err) = state.webhooks.send_email(&email).await {
            tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "onboarding fee failure email failed"
            );
        }
    }
}

/// Staff are told when this outcome moved the fee into `fee_failed` (not on
/// a repeated failure).
async fn after_event(state: &AppState, saved: Option<&Outcome>) {
    if let Some(outcome) = saved
        && outcome
            .events
            .iter()
            .any(|event| event.kind == EventKind::FeeFailed)
    {
        notify_failed(state, &outcome.commitment).await;
    }
}

/// The fee invoice to charge: the recorded one, or for `notice_sent` the one
/// a stopped run left (found by metadata) or a new draft.
async fn fee_invoice(
    state: &AppState,
    commitment: &Commitment,
    customer: &str,
    payment_method: Option<&str>,
) -> Result<Value, FeeError> {
    let company_id = commitment.company_id;
    let stripe = &state.stripe;
    let deadline = deadline_key(commitment.deadline_at);
    match (commitment.status, commitment.stripe_invoice_id.as_deref()) {
        (Status::FeeInvoiced, Some(id)) => stripe.invoice(id).await.map_err(|e| stripe_error(&e)),
        (Status::NoticeSent, _) => {
            let existing = stripe
                .customer_invoices(customer)
                .await
                .map_err(|e| stripe_error(&e))?
                .into_iter()
                .find(|invoice| is_fee_invoice(invoice, company_id, &deadline));
            if let Some(invoice) = existing {
                return Ok(invoice);
            }
            let recorded = store::fee_invoices_recorded(&state.pool, company_id).await?;
            let description = format!(
                "Granite Manager onboarding fee: onboarding not completed by {} (Terms of Service, Section 11)",
                crate::billing::reminder_emails::format_local_date(commitment.deadline_at)
            );
            stripe
                .create_fee_invoice(&FeeInvoice {
                    customer,
                    company_id,
                    deadline: &deadline,
                    payment_method,
                    description: &description,
                    automatic_tax: state.signup.automatic_tax,
                    idempotency_key: &create_key(company_id, &deadline, recorded),
                })
                .await
                .map_err(|e| stripe_error(&e))
        }
        _ => Err(FeeError::Held("no_invoice")),
    }
}

/// Pays the open fee invoice; the invoice as Stripe has it after. A card
/// decline is `fee_failed` (staff are told) and gives `None`. Any other
/// failure is an error the next daily run retries, unless Stripe has the
/// invoice paid ("already paid", a lost answer).
async fn pay(
    state: &AppState,
    company_id: i32,
    invoice_id: &str,
    key: &str,
    now: NaiveDateTime,
) -> Result<Option<Value>, FeeError> {
    let stripe = &state.stripe;
    match stripe.pay_invoice(invoice_id, &format!("{key}:pay")).await {
        Ok(invoice) => Ok(Some(invoice)),
        Err(err) if is_decline(&err) => {
            let saved = record_event(
                &state.pool,
                company_id,
                invoice_id,
                InvoiceEvent::Failed,
                now,
            )
            .await?;
            after_event(state, saved.as_ref()).await;
            Ok(None)
        }
        Err(err) => {
            let fresh = stripe
                .invoice(invoice_id)
                .await
                .map_err(|_| stripe_error(&err))?;
            if fresh["status"] == "paid" {
                Ok(Some(fresh))
            } else {
                Err(stripe_error(&err))
            }
        }
    }
}

/// Creates (or finds) the fee invoice, records it, then finalizes and pays
/// it. Also finishes an invoice a crash left behind (`fee_invoiced` with a
/// draft, or an open one never attempted), or withdraws it when the fee may
/// no longer be charged ([`withdrawal`]). [`FeeError::Held`] without asking
/// Stripe anything when a `notice_sent` fee may not be charged now.
pub async fn charge(
    state: &AppState,
    stored: &Stored,
    now: NaiveDateTime,
) -> Result<Charged, FeeError> {
    let commitment = &stored.commitment;
    let company_id = commitment.company_id;
    let pool = &state.pool;
    let stripe = &state.stripe;
    let facts = charge_facts(state, company_id).await?;
    let blocked = withdrawal(commitment, &facts);
    if let (Status::FeeInvoiced, Some(id), Some(reason)) = (
        commitment.status,
        commitment.stripe_invoice_id.as_deref(),
        blocked,
    ) {
        return withdraw(state, company_id, id, reason, now).await;
    }
    let held = blocked.map(Withdrawal::as_str).or_else(|| {
        hold_reason(
            commitment,
            facts.charging_enabled,
            facts.consent_on_file,
            now,
        )
    });
    if let Some(reason) = held {
        return Err(FeeError::Held(reason));
    }
    let record = MySqlBillingStore(pool).by_company(company_id).await?;
    let Some(customer) = record.as_ref().and_then(|r| r.stripe_customer_id.clone()) else {
        return Err(FeeError::NoCustomer);
    };
    if !stripe.is_enabled() {
        return Err(FeeError::Stripe("not_configured".into()));
    }
    let payment_method = record
        .as_ref()
        .and_then(|r| r.stripe_payment_method_id.as_deref());
    let mut invoice = fee_invoice(state, commitment, &customer, payment_method).await?;
    let Some(invoice_id) = invoice["id"].as_str().map(str::to_string) else {
        return Err(FeeError::Stripe("invalid_response".into()));
    };
    let key = format!("onboarding-fee:{invoice_id}");
    if invoice["status"] == "draft" && !has_lines(&invoice) {
        stripe
            .create_invoice_item(
                &customer,
                &invoice_id,
                commitment.fee_cents,
                "Onboarding fee (Terms of Service, Section 11)",
                &format!("{key}:item"),
            )
            .await
            .map_err(|e| stripe_error(&e))?;
    }
    if commitment.status == Status::NoticeSent {
        let outcome = invoice_created(commitment, &invoice_id, now);
        store::save(pool, stored.version, &outcome, now).await?;
    }
    if invoice["status"] == "draft" {
        if let Some(stopped) = stop_unless_chargeable(state, company_id, &invoice_id, now).await? {
            return Ok(stopped);
        }
        invoice = stripe
            .finalize_invoice(&invoice_id, &format!("{key}:finalize"))
            .await
            .map_err(|e| stripe_error(&e))?;
    }
    if invoice["status"] == "open" && invoice["attempted"] != true {
        if let Some(stopped) = stop_unless_chargeable(state, company_id, &invoice_id, now).await? {
            return Ok(stopped);
        }
        let Some(answered) = pay(state, company_id, &invoice_id, &key, now).await? else {
            return Ok(Charged::Failed);
        };
        invoice = answered;
    }
    if invoice["status"] == "paid" {
        record_event(pool, company_id, &invoice_id, InvoiceEvent::Paid, now).await?;
        return Ok(Charged::Paid);
    }
    Ok(Charged::Processing)
}

/// A Stripe invoice that is an onboarding fee (`metadata.kind`): never part
/// of the subscription's balance or the access gate.
pub fn is_fee_invoice_object(invoice: &Value) -> bool {
    invoice["metadata"]["kind"] == FEE_KIND
}

/// An `invoice.*` event about a fee invoice (`metadata.kind`).
pub fn is_fee_event(event: &Value) -> bool {
    event["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("invoice."))
        && is_fee_invoice_object(&event["data"]["object"])
}

/// `invoice.paid`, `invoice.payment_failed` and `invoice.voided` of the fee
/// invoice move the commitment (matched by `metadata.company_id` and the
/// stored invoice id); other invoice events are ignored. The first failure
/// tells staff. Never touches subscription billing.
pub async fn apply_webhook(state: &AppState, event: &Value) -> Result<(), FeeError> {
    let kind = match event["type"].as_str() {
        Some("invoice.paid") => InvoiceEvent::Paid,
        Some("invoice.payment_failed") => InvoiceEvent::Failed,
        Some("invoice.voided") => InvoiceEvent::Voided,
        _ => return Ok(()),
    };
    let invoice = &event["data"]["object"];
    let (Some(company_id), Some(invoice_id)) = (
        positive_int(&invoice["metadata"]["company_id"]),
        invoice["id"].as_str(),
    ) else {
        return Ok(());
    };
    let saved = record_event(&state.pool, company_id, invoice_id, kind, billing::now()).await?;
    after_event(state, saved.as_ref()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::stripe::StripeFailure;
    use crate::onboarding_call::commitment::{new_commitment, notice_sent};
    use serde_json::json;

    fn failure(status: u16, kind: Option<&str>, code: &str) -> StripeFailure {
        StripeFailure {
            message: "Stripe said no".into(),
            code: code.into(),
            status,
            kind: kind.map(str::to_string),
        }
    }

    #[test]
    fn only_a_card_decline_fails_the_fee() {
        let declines = [
            StripeError::Api(failure(402, Some("card_error"), "card_declined")),
            StripeError::Api(failure(402, None, "http_402")),
            StripeError::Api(failure(400, Some("card_error"), "expired_card")),
        ];
        assert!(declines.iter().all(is_decline));
        let retried = [
            StripeError::Api(failure(
                400,
                Some("idempotency_error"),
                "idempotency_key_in_use",
            )),
            StripeError::Api(failure(
                400,
                Some("invalid_request_error"),
                "invoice_already_paid",
            )),
            StripeError::Server(failure(500, Some("api_error"), "http_500")),
            StripeError::Server(failure(429, None, "rate_limit")),
            StripeError::Missing(failure(
                404,
                Some("invalid_request_error"),
                "resource_missing",
            )),
            StripeError::Request("timed out".into()),
            StripeError::NotConfigured,
        ];
        assert!(!retried.iter().any(is_decline));
    }

    #[test]
    fn the_deadline_key_is_the_local_day_the_consent_named() {
        let end_of_nov_22 =
            NaiveDateTime::parse_from_str("2026-11-23 04:59:59", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(deadline_key(end_of_nov_22), "2026-11-22");
    }

    #[test]
    fn a_new_invoice_after_a_withdrawn_one_gets_a_new_creation_key() {
        assert_eq!(
            create_key(7, "2026-11-22", 0),
            "onboarding-fee:7:2026-11-22"
        );
        assert_eq!(
            create_key(7, "2026-11-22", 1),
            "onboarding-fee:7:2026-11-22:1"
        );
        assert_ne!(
            create_key(7, "2026-11-22", 2),
            create_key(7, "2026-11-22", 1)
        );
    }

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    /// Notice sent 2026-10-31 12:00: charge after 2026-11-07 12:00.
    fn noticed() -> Commitment {
        let c = new_commitment(7, at("2026-10-01 10:00"), at("2026-10-31 10:00"), 60_000);
        notice_sent(&c.commitment, at("2026-10-31 12:00")).commitment
    }

    #[test]
    fn the_fee_is_held_unless_charging_consent_notice_and_charge_date_all_hold() {
        let due = at("2026-11-07 12:00");
        let c = noticed();
        assert_eq!(hold_reason(&c, true, true, due), None);
        let invoiced = Commitment {
            status: Status::FeeInvoiced,
            ..c.clone()
        };
        assert_eq!(hold_reason(&invoiced, true, true, due), None);
        assert_eq!(hold_reason(&c, false, true, due), Some("charging_off"));
        assert_eq!(
            hold_reason(&invoiced, false, true, due),
            Some("charging_off")
        );
        assert_eq!(hold_reason(&c, true, false, due), Some("no_consent"));
        assert_eq!(
            hold_reason(&c, true, true, at("2026-11-07 11:59")),
            Some("before_charge_date")
        );
        let unsent = Commitment {
            notice_sent_at: None,
            ..c.clone()
        };
        assert_eq!(hold_reason(&unsent, true, true, due), Some("no_notice"));
        let no_date = Commitment {
            charge_after: None,
            ..c.clone()
        };
        assert_eq!(
            hold_reason(&no_date, true, true, due),
            Some("before_charge_date")
        );
        for status in [
            Status::Pending,
            Status::Booked,
            Status::Suspended,
            Status::FeeFailed,
            Status::FeePaid,
            Status::Waived,
            Status::Void,
        ] {
            let other = Commitment {
                status,
                ..c.clone()
            };
            assert_eq!(hold_reason(&other, true, true, due), Some("status"));
        }
        assert_eq!(FeeError::Held("charging_off").code(), "held_charging_off");
    }

    #[test]
    fn only_invoice_events_tagged_as_the_fee_are_routed_here() {
        let fee = |kind: &str| json!({ "type": kind, "data": { "object": { "metadata": { "kind": "onboarding_fee" } } } });
        assert!(is_fee_event(&fee("invoice.paid")));
        assert!(is_fee_event(&fee("invoice.finalized")));
        assert!(!is_fee_event(&fee("customer.subscription.updated")));
        assert!(!is_fee_event(&json!({
            "type": "invoice.paid", "data": { "object": { "metadata": { "company_id": "7" } } }
        })));
    }

    #[test]
    fn a_fee_invoice_is_found_again_by_company_and_deadline_unless_void() {
        let invoice = |status: &str, deadline: &str, company: &str| {
            json!({ "id": "in_1", "status": status, "metadata": {
                "kind": "onboarding_fee", "deadline": deadline, "company_id": company } })
        };
        assert!(is_fee_invoice(
            &invoice("open", "2026-11-20", "7"),
            7,
            "2026-11-20"
        ));
        assert!(is_fee_invoice(
            &invoice("draft", "2026-11-20", "7"),
            7,
            "2026-11-20"
        ));
        assert!(!is_fee_invoice(
            &invoice("void", "2026-11-20", "7"),
            7,
            "2026-11-20"
        ));
        assert!(!is_fee_invoice(
            &invoice("open", "2026-11-21", "7"),
            7,
            "2026-11-20"
        ));
        assert!(!is_fee_invoice(
            &invoice("open", "2026-11-20", "8"),
            7,
            "2026-11-20"
        ));
        assert!(!has_lines(&invoice("draft", "2026-11-20", "7")));
        assert!(has_lines(
            &json!({ "lines": { "data": [{ "id": "il_1" }] } })
        ));
    }
}
