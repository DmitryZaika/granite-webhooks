//! The onboarding advance in Stripe (Terms section 11) and its webhook.
//!
//! The advance is one month of the subscription, added as a pending invoice
//! item on the subscription within the last [`ITEM_WINDOW_DAYS`] before the
//! charge the notice named, priced on the base product (tax exclusive, like
//! the catalog); Stripe sweeps it into that subscription invoice, so it is
//! charged (and dunned) with it. Once that invoice is paid the advance
//! line's total there (its amount and its tax) is credited to the
//! customer's balance, which Stripe applies to the next invoice. Nothing is
//! paid more overall, only earlier. If the subscription ends while that
//! invoice is unpaid, the advance line is credited off it (a credit note):
//! the month it pays for never comes.
//!
//! Every item creation is keyed by company, deadline and the items made so
//! far; a run that stopped halfway finds its item again by metadata before
//! adding one, and the commitment records the item before anything else
//! happens to it. Right before the item is added, and while it is pending,
//! everything is read again ([`withdrawal`]): a pending item that may no
//! longer be charged is deleted. The credit is keyed by the invoice, and
//! the customer's balance transactions are read first, so a replay, a late
//! webhook and the daily run never credit twice.
//!
//! The work that reads or changes billing state runs under the company lock:
//! [`add_item`] and [`settle_item`] take it, [`settle_item_locked`] and
//! [`invoice_paid`] expect it held.
//!
//! [`ITEM_WINDOW_DAYS`]: super::commitment::ITEM_WINDOW_DAYS

use super::commitment::{
    self, AdvanceTerms, CHARGE_LATE_HOURS, ChargeFacts, Commitment, Outcome, Status, Withdrawal,
    advance_scheduled, advance_withdrawn, credited, drop_notice, invoice_ended, item_credited_off,
    item_invoiced, item_open, item_withdrawn, withdrawal,
};
use super::store::{self, SaveError, Stored};
use crate::billing::cancellation::{self, CancellationError, is_live_subscription_status};
use crate::billing::catalog::{BASE_LOOKUP_KEY, SIGNUP_TZ};
use crate::billing::lock::{CompanyLock, LockError};
use crate::billing::reminder_emails::{
    RenderedEmail, escape_html, format_local_date, format_money, html_paragraph, wrap_html,
};
use crate::billing::store::{BillingStore, MySqlBillingStore};
use crate::billing::stripe::{
    ADVANCE_CREDIT_KIND, ADVANCE_CREDIT_NOTE_KIND, ADVANCE_ITEM_KIND, AdvanceItem, Stripe,
    StripeError, positive_int, stripe_id, subscription_snapshot,
};
use crate::state::AppState;
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::time::Instant;

/// One Stripe call of the advance may take this long, so a run stays inside
/// its share of the time-triggered tick.
const STRIPE_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum AdvanceError {
    /// No Stripe customer: reported, tried again the next day.
    #[error("the company has no Stripe customer")]
    NoCustomer,
    /// Stripe refused or failed; the code only. Tried again the next day.
    #[error("Stripe failed: {0}")]
    Stripe(String),
    /// The run's time is up: stopped between two Stripe calls; the next run
    /// resumes with the same idempotency keys.
    #[error("the run's time is up")]
    OutOfTime,
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Save(#[from] SaveError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl AdvanceError {
    /// Safe to log and report.
    pub fn code(&self) -> String {
        match self {
            Self::NoCustomer => "no_stripe_customer".into(),
            Self::Stripe(code) => code.clone(),
            Self::OutOfTime => "out_of_time".into(),
            Self::Lock(LockError::Busy) => "busy".into(),
            Self::Save(SaveError::Conflict) => "conflict".into(),
            Self::Lock(LockError::Database(_))
            | Self::Save(SaveError::Database(_))
            | Self::Database(_) => "database".into(),
        }
    }

    /// Worth retrying on the next tick (not the next day).
    pub const fn retry_soon(&self) -> bool {
        matches!(
            self,
            Self::OutOfTime | Self::Lock(_) | Self::Save(_) | Self::Database(_)
        )
    }
}

impl From<CancellationError> for AdvanceError {
    fn from(err: CancellationError) -> Self {
        match err {
            CancellationError::Stripe(err) => stripe_error(&err),
            CancellationError::Database(err) => Self::Database(err),
            CancellationError::Lock(err) => Self::Lock(err),
            CancellationError::Invalid(_) | CancellationError::Conflict(_) => {
                Self::Stripe("cancellation_refused".into())
            }
        }
    }
}

fn stripe_error(err: &StripeError) -> AdvanceError {
    AdvanceError::Stripe(err.code().to_string())
}

/// `YYYY-MM-DD` of the deadline, its day in `SIGNUP_TZ` (the day the
/// consent named): `metadata[deadline]` and the item's idempotency key.
pub fn deadline_key(deadline_at: NaiveDateTime) -> String {
    SIGNUP_TZ
        .from_utc_datetime(&deadline_at)
        .format("%Y-%m-%d")
        .to_string()
}

/// The idempotency key of the item's creation; after `recorded` items it
/// carries their count, so Stripe makes a new one.
fn item_key(company_id: i32, deadline: &str, recorded: i64) -> String {
    if recorded > 0 {
        format!("onboarding-advance:{company_id}:{deadline}:{recorded}")
    } else {
        format!("onboarding-advance:{company_id}:{deadline}")
    }
}

fn credit_key(company_id: i32, invoice_id: &str) -> String {
    format!("onboarding-advance:{company_id}:{invoice_id}:credit")
}

fn credit_note_key(company_id: i32, invoice_id: &str) -> String {
    format!("onboarding-advance:{company_id}:{invoice_id}:credit-note")
}

/// `From:` of the onboarding emails (`BILLING_EMAIL_FROM_ADDRESS`).
pub(super) fn from_address() -> String {
    crate::billing::from_address()
}

pub(super) fn advance_stripe(state: &AppState) -> Stripe {
    let mut stripe = state.stripe.clone();
    stripe.request_timeout = STRIPE_CALL_TIMEOUT;
    stripe
}

/// Before every Stripe call: stop once the run's time is up.
fn in_time(until: Instant) -> Result<(), AdvanceError> {
    if Instant::now() >= until {
        Err(AdvanceError::OutOfTime)
    } else {
        Ok(())
    }
}

fn unix(value: &Value) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(value.as_i64()?, 0).map(|at| at.naive_utc())
}

/// What the advance needs from the live subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSubscription {
    pub id: String,
    /// Trialing, active or past due.
    pub live: bool,
    /// `cancel_at_period_end` or `cancel_at` is set.
    pub cancel_scheduled: bool,
    /// The trial end while trialing, else the current period end.
    pub next_charge_at: Option<NaiveDateTime>,
    pub anchor: Option<NaiveDateTime>,
    /// Unit amount x quantity of every item, before tax.
    pub monthly_cents: i64,
    /// The product of the base item's price (`granite_base_monthly_v1`):
    /// the advance item is priced on it.
    pub base_product: Option<String>,
}

pub fn live_subscription(subscription: &Value) -> LiveSubscription {
    let snapshot = subscription_snapshot(subscription);
    let next_charge_at = if snapshot.status == "trialing" {
        snapshot.trial_end.or(snapshot.period_end)
    } else {
        snapshot.period_end
    };
    LiveSubscription {
        live: is_live_subscription_status(&snapshot.status),
        cancel_scheduled: snapshot.cancel_at.is_some(),
        next_charge_at,
        anchor: unix(&subscription["billing_cycle_anchor"]),
        monthly_cents: snapshot
            .items
            .iter()
            .filter_map(|item| Some(item.unit_amount_cents? * item.quantity))
            .sum(),
        base_product: subscription["items"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["price"]["lookup_key"] == BASE_LOOKUP_KEY)
            .and_then(|item| stripe_id(&item["price"]["product"])),
        id: snapshot.id,
    }
}

/// The company's Stripe customer and subscription ids.
async fn billing_ids(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<(Option<String>, Option<String>), AdvanceError> {
    let record = MySqlBillingStore(pool).by_company(company_id).await?;
    Ok(record.map_or((None, None), |r| {
        (r.stripe_customer_id, r.stripe_subscription_id)
    }))
}

/// The live subscription; `None` without one (never made, or gone).
async fn read_subscription(
    stripe: &Stripe,
    subscription_id: Option<&str>,
    until: Instant,
) -> Result<Option<LiveSubscription>, AdvanceError> {
    let Some(id) = subscription_id else {
        return Ok(None);
    };
    in_time(until)?;
    match stripe.subscription(id).await {
        Ok(subscription) => Ok(Some(live_subscription(&subscription))),
        Err(StripeError::Missing(_)) => Ok(None),
        Err(err) => Err(stripe_error(&err)),
    }
}

/// What the notice can state now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeTerms {
    Terms(AdvanceTerms),
    /// No running subscription: nothing is ever noticed or charged.
    NoSubscription,
    /// Stripe ends the subscription: the cancellation suspends first.
    CancelScheduled,
}

/// The terms of a notice at `now`, from the live subscription
/// ([`commitment::advance_terms`]).
pub async fn notice_terms(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    until: Instant,
) -> Result<NoticeTerms, AdvanceError> {
    let stripe = &advance_stripe(state);
    if !stripe.is_enabled() {
        return Err(AdvanceError::Stripe("not_configured".into()));
    }
    let (_, subscription_id) = billing_ids(&state.pool, company_id).await?;
    let Some(live) = read_subscription(stripe, subscription_id.as_deref(), until).await? else {
        return Ok(NoticeTerms::NoSubscription);
    };
    if !live.live {
        return Ok(NoticeTerms::NoSubscription);
    }
    if live.cancel_scheduled {
        return Ok(NoticeTerms::CancelScheduled);
    }
    if live.monthly_cents <= 0 {
        return Err(AdvanceError::Stripe("no_amount".into()));
    }
    let next = live
        .next_charge_at
        .ok_or_else(|| AdvanceError::Stripe("no_next_charge".into()))?;
    commitment::advance_terms(live.monthly_cents, live.anchor, next, now)
        .map(NoticeTerms::Terms)
        .ok_or_else(|| AdvanceError::Stripe("no_charge_date".into()))
}

/// Applies `change` to the commitment as stored (read again on a conflict);
/// the outcome saved, `None` when it changed nothing.
pub(crate) async fn apply(
    pool: &MySqlPool,
    company_id: i32,
    now: NaiveDateTime,
    change: impl Fn(&Commitment) -> Outcome,
) -> Result<Option<Outcome>, AdvanceError> {
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

async fn charge_facts(
    state: &AppState,
    company_id: i32,
    live: Option<&LiveSubscription>,
) -> Result<ChargeFacts, AdvanceError> {
    let pool = &state.pool;
    Ok(ChargeFacts {
        charging_enabled: state.onboarding_call.advance_charging,
        consent_on_file: store::consent_on_file(pool, company_id).await?,
        cancellation_requested_at: store::cancellation_requested_at(pool, company_id).await?,
        subscription_live: live.is_some_and(|live| live.live),
        cancel_scheduled: live.is_some_and(|live| live.cancel_scheduled),
        next_charge_at: live.and_then(|live| live.next_charge_at),
    })
}

/// Our advance item for `company_id` and `deadline` (metadata), if any.
fn is_advance_item(item: &Value, company_id: i32, deadline: &str) -> bool {
    let metadata = &item["metadata"];
    metadata["kind"] == ADVANCE_ITEM_KIND
        && metadata["deadline"] == deadline
        && positive_int(&metadata["company_id"]) == Some(company_id)
        && item["deleted"] != true
}

/// `Advance payment for March 1, 2027 to April 1, 2027`.
pub fn item_description(terms: &AdvanceTerms) -> String {
    format!(
        "Advance payment for {} to {}",
        format_local_date(terms.prepaid_from),
        format_local_date(terms.prepaid_until)
    )
}

/// `Credit for the advance payment of February 28, 2027 to March 28, 2027`.
fn credit_description(terms: Option<&AdvanceTerms>) -> String {
    terms.map_or_else(
        || "Credit for the advance payment".to_string(),
        |terms| {
            format!(
                "Credit for the advance payment of {} to {}",
                format_local_date(terms.prepaid_from),
                format_local_date(terms.prepaid_until)
            )
        },
    )
}

/// What [`add_item`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// A new item is in Stripe.
    Created,
    /// An item a stopped run left was found and recorded.
    Found,
    /// The advance may not be charged now ([`withdrawal`]); the commitment
    /// moved on accordingly (or waits).
    Withdrawn(Withdrawal),
    /// The charge date passed with no item: the notice was dropped.
    Missed,
    /// Nothing to do any more (the commitment changed meanwhile).
    Nothing,
}

/// `Due::AddItem`: puts the advance's item on the subscription, under the
/// company lock.
pub async fn add_item(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Added, AdvanceError> {
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = add_item_locked(state, company_id, now, until).await;
    lock.release().await;
    result
}

async fn add_item_locked(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Added, AdvanceError> {
    let pool = &state.pool;
    let stripe = &advance_stripe(state);
    let Some(stored) = store::load(pool, company_id).await? else {
        return Ok(Added::Nothing);
    };
    let c = &stored.commitment;
    let (Status::NoticeSent, None, Some(terms)) = (c.status, &c.advance_item_id, c.terms) else {
        return Ok(Added::Nothing);
    };
    let (Some(customer), subscription_id) = billing_ids(pool, company_id).await? else {
        return Err(AdvanceError::NoCustomer);
    };
    if !stripe.is_enabled() {
        return Err(AdvanceError::Stripe("not_configured".into()));
    }
    let deadline = deadline_key(c.deadline_at);
    in_time(until)?;
    let existing = stripe
        .customer_invoice_items(&customer)
        .await
        .map_err(|e| stripe_error(&e))?
        .into_iter()
        .find(|item| is_advance_item(item, company_id, &deadline));
    if let Some(item) = existing
        && let Some(item_id) = stripe_id(&item)
    {
        let invoice = stripe_id(&item["invoice"]);
        apply(pool, company_id, now, |c| {
            advance_scheduled(c, &item_id, invoice.as_deref(), now)
        })
        .await?;
        tracing::info!(company_id, "onboarding advance item found again");
        return Ok(Added::Found);
    }
    if now >= terms.charge_at {
        apply(pool, company_id, now, |c| {
            drop_notice(c, "charge_missed", now)
        })
        .await?;
        return Ok(Added::Missed);
    }
    let live = read_subscription(stripe, subscription_id.as_deref(), until).await?;
    let facts = charge_facts(state, company_id, live.as_ref()).await?;
    if let Some(reason) = withdrawal(c, &facts) {
        apply(pool, company_id, now, |c| advance_withdrawn(c, reason, now)).await?;
        tracing::info!(
            company_id,
            reason = reason.as_str(),
            "onboarding advance not added"
        );
        return Ok(Added::Withdrawn(reason));
    }
    let Some(live) = live else {
        return Ok(Added::Nothing);
    };
    let product = live
        .base_product
        .clone()
        .ok_or_else(|| AdvanceError::Stripe("no_base_product".into()))?;
    let recorded = store::advance_items_recorded(pool, company_id).await?;
    in_time(until)?;
    let item = stripe
        .create_advance_item(&AdvanceItem {
            customer: &customer,
            subscription: &live.id,
            company_id,
            deadline: &deadline,
            amount_cents: terms.advance_cents,
            product: &product,
            description: &item_description(&terms),
            idempotency_key: &item_key(company_id, &deadline, recorded),
        })
        .await
        .map_err(|e| stripe_error(&e))?;
    let item_id =
        stripe_id(&item).ok_or_else(|| AdvanceError::Stripe("invalid_response".into()))?;
    apply(pool, company_id, now, |c| {
        advance_scheduled(c, &item_id, None, now)
    })
    .await?;
    let kept = store::load(pool, company_id)
        .await?
        .is_some_and(|stored| stored.commitment.advance_item_id.as_deref() == Some(&item_id));
    if !kept {
        // The commitment moved on meanwhile: the item must not stay.
        in_time(until)?;
        match stripe.delete_invoice_item(&item_id).await {
            Ok(_) | Err(StripeError::Missing(_)) => {}
            Err(err) => return Err(stripe_error(&err)),
        }
        return Ok(Added::Nothing);
    }
    tracing::info!(company_id, "onboarding advance item added");
    Ok(Added::Created)
}

/// What [`settle_item`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// No open item.
    Nothing,
    /// Pending or on an unpaid invoice: waits.
    Waiting,
    /// Deleted (no longer wanted) or found gone.
    Withdrawn,
    /// Its invoice was paid and the month credited back.
    Credited,
    /// Its invoice ended unpaid (void, uncollectible).
    Ended,
    /// The subscription ended with it on an unpaid invoice: credited off.
    CreditedOff,
}

/// `Due::SettleItem`, under the company lock.
pub async fn settle_item(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = settle_item_locked(state, company_id, now, until).await;
    lock.release().await;
    result
}

/// Whether the recorded item may still be deleted (pending, on a draft
/// invoice, or gone).
///
/// Read from Stripe under the company lock. On a finalized invoice it may
/// not: that invoice is recorded (the advance is then credited back once
/// paid). True without an open item.
pub async fn item_removable(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
) -> Result<bool, AdvanceError> {
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = item_removable_locked(state, company_id, now).await;
    lock.release().await;
    result
}

async fn item_removable_locked(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
) -> Result<bool, AdvanceError> {
    let Some(stored) = store::load(&state.pool, company_id).await? else {
        return Ok(true);
    };
    let c = &stored.commitment;
    let (true, Some(item_id)) = (item_open(c), c.advance_item_id.as_deref()) else {
        return Ok(true);
    };
    let stripe = &advance_stripe(state);
    if !stripe.is_enabled() {
        return Err(AdvanceError::Stripe("not_configured".into()));
    }
    let item = match stripe.invoice_item(item_id).await {
        Ok(item) if item["deleted"] != true => item,
        Ok(_) | Err(StripeError::Missing(_)) => return Ok(true),
        Err(err) => return Err(stripe_error(&err)),
    };
    let Some(invoice_id) = stripe_id(&item["invoice"]) else {
        return Ok(true);
    };
    let status = match stripe.invoice(&invoice_id).await {
        Ok(invoice) => invoice["status"].as_str().map(str::to_string),
        // A deleted draft takes its items back to pending.
        Err(StripeError::Missing(_)) => return Ok(true),
        Err(err) => return Err(stripe_error(&err)),
    };
    if status.as_deref() == Some("draft") {
        return Ok(true);
    }
    apply(&state.pool, company_id, now, |c| {
        item_invoiced(c, &invoice_id, now)
    })
    .await?;
    tracing::info!(
        company_id,
        "onboarding advance item found on a finalized invoice"
    );
    Ok(false)
}

/// Deletes the item (pending, or on a draft invoice), then forgets it.
async fn delete_item(
    state: &AppState,
    company_id: i32,
    item_id: &str,
    reason: &str,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    in_time(until)?;
    match advance_stripe(state).delete_invoice_item(item_id).await {
        Ok(_) | Err(StripeError::Missing(_)) => {}
        Err(err) => return Err(stripe_error(&err)),
    }
    apply(&state.pool, company_id, now, |c| {
        if c.advance_item_id.as_deref() == Some(item_id) {
            item_withdrawn(c, reason, now)
        } else {
            unchanged(c)
        }
    })
    .await?;
    tracing::info!(company_id, reason, "onboarding advance item deleted");
    Ok(Settled::Withdrawn)
}

fn unchanged(c: &Commitment) -> Outcome {
    Outcome {
        commitment: c.clone(),
        events: Vec::new(),
    }
}

/// Settles the recorded item; the caller holds the company lock.
///
/// A pending item is kept while the advance may be charged ([`withdrawal`]), else
/// deleted (the commitment first moves on); an item on an invoice records
/// it, and once that invoice is paid credits the month back ([`credit`]);
/// an invoice that ended unpaid forgets it.
pub async fn settle_item_locked(
    state: &AppState,
    company_id: i32,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    let pool = &state.pool;
    let stripe = &advance_stripe(state);
    let Some(stored) = store::load(pool, company_id).await? else {
        return Ok(Settled::Nothing);
    };
    let c = &stored.commitment;
    let (true, Some(item_id)) = (item_open(c), c.advance_item_id.clone()) else {
        return Ok(Settled::Nothing);
    };
    if !stripe.is_enabled() {
        return Err(AdvanceError::Stripe("not_configured".into()));
    }
    in_time(until)?;
    let item = match stripe.invoice_item(&item_id).await {
        Ok(item) if item["deleted"] != true => item,
        Ok(_) | Err(StripeError::Missing(_)) => {
            apply(pool, company_id, now, |c| {
                item_withdrawn(c, "item_missing", now)
            })
            .await?;
            return Ok(Settled::Withdrawn);
        }
        Err(err) => return Err(stripe_error(&err)),
    };
    let Some(invoice_id) = stripe_id(&item["invoice"]) else {
        return settle_pending(state, &stored, &item_id, now, until).await;
    };
    if c.stripe_invoice_id.as_deref() != Some(&invoice_id) {
        apply(pool, company_id, now, |c| {
            item_invoiced(c, &invoice_id, now)
        })
        .await?;
    }
    in_time(until)?;
    let invoice = match stripe.invoice(&invoice_id).await {
        Ok(invoice) => invoice,
        Err(StripeError::Missing(_)) => {
            // A deleted draft takes its items back to pending.
            return Ok(Settled::Waiting);
        }
        Err(err) => return Err(stripe_error(&err)),
    };
    match invoice["status"].as_str() {
        Some("paid") => {
            in_time(until)?;
            let lines = stripe
                .invoice_lines(&invoice_id)
                .await
                .map_err(|e| stripe_error(&e))?;
            let amount = advance_line_total(&lines, &item_id)
                .or_else(|| item["amount"].as_i64())
                .or_else(|| c.terms.map(|t| t.advance_cents));
            let ended = invoice_subscription_ended(stripe, &invoice, until).await?;
            let paid = PaidAdvance {
                invoice_id: &invoice_id,
                amount_cents: amount.unwrap_or_default(),
                subscription_ended: ended,
            };
            credit(state, c, &paid, now, until).await
        }
        Some(status @ ("void" | "uncollectible")) => {
            apply(pool, company_id, now, |c| {
                invoice_ended(c, &invoice_id, status, now)
            })
            .await?;
            Ok(Settled::Ended)
        }
        Some("open") => {
            if invoice_subscription_ended(stripe, &invoice, until).await? {
                credit_off(state, c, &invoice_id, &item_id, now, until).await
            } else {
                Ok(Settled::Waiting)
            }
        }
        Some("draft") => {
            if c.status != Status::AdvanceScheduled {
                return delete_item(state, company_id, &item_id, "not_wanted", now, until).await;
            }
            if !invoice_subscription_ended(stripe, &invoice, until).await? {
                return Ok(Settled::Waiting);
            }
            let reason = Withdrawal::SubscriptionEnded;
            apply(pool, company_id, now, |c| advance_withdrawn(c, reason, now)).await?;
            delete_item(state, company_id, &item_id, reason.as_str(), now, until).await
        }
        _ => Ok(Settled::Waiting),
    }
}

/// The subscription `invoice` belongs to has ended (`canceled`,
/// `incomplete_expired`, or gone); false for an invoice of no subscription.
async fn invoice_subscription_ended(
    stripe: &Stripe,
    invoice: &Value,
    until: Instant,
) -> Result<bool, AdvanceError> {
    let Some(id) = stripe_id(&invoice["parent"]["subscription_details"]["subscription"])
        .or_else(|| stripe_id(&invoice["subscription"]))
    else {
        return Ok(false);
    };
    in_time(until)?;
    match stripe.subscription(&id).await {
        Ok(subscription) => Ok(matches!(
            subscription["status"].as_str(),
            Some("canceled" | "incomplete_expired")
        )),
        Err(StripeError::Missing(_)) => Ok(true),
        Err(err) => Err(stripe_error(&err)),
    }
}

/// The subscription ended with the advance on its unpaid invoice: the
/// advance line (with its tax) is credited off that invoice once (a credit
/// note already there is recorded instead), the commitment voids, and staff
/// are told.
async fn credit_off(
    state: &AppState,
    c: &Commitment,
    invoice_id: &str,
    item_id: &str,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    let company_id = c.company_id;
    let stripe = &advance_stripe(state);
    in_time(until)?;
    let existing = stripe
        .invoice_credit_notes(invoice_id)
        .await
        .map_err(|e| stripe_error(&e))?
        .into_iter()
        .find(|note| {
            note["metadata"]["kind"] == ADVANCE_CREDIT_NOTE_KIND && note["status"] != "void"
        });
    let (note_id, created) = if let Some(id) = existing.as_ref().and_then(stripe_id) {
        (id, None)
    } else {
        in_time(until)?;
        let lines = stripe
            .invoice_lines(invoice_id)
            .await
            .map_err(|e| stripe_error(&e))?;
        let line_id = advance_line(&lines, item_id)
            .and_then(stripe_id)
            .ok_or_else(|| AdvanceError::Stripe("no_advance_line".into()))?;
        in_time(until)?;
        let note = stripe
            .credit_advance_line(
                invoice_id,
                &line_id,
                company_id,
                &credit_off_memo(c.terms.as_ref()),
                &credit_note_key(company_id, invoice_id),
            )
            .await
            .map_err(|e| stripe_error(&e))?;
        let id = stripe_id(&note).ok_or_else(|| AdvanceError::Stripe("invalid_response".into()))?;
        (
            id,
            Some(note["total"].as_i64().or_else(|| note["amount"].as_i64())),
        )
    };
    apply(&state.pool, company_id, now, |c| {
        item_credited_off(c, invoice_id, &note_id, now)
    })
    .await?;
    tracing::info!(
        company_id,
        "onboarding advance credited off after the subscription ended"
    );
    if let Some(amount) = created {
        let alert = StaffAlert::CreditedOff {
            invoice_id,
            amount_cents: amount,
        };
        notify_staff(state, company_id, c.terms.as_ref(), &alert).await;
    }
    Ok(Settled::CreditedOff)
}

/// `Advance payment for <months> credited: the subscription ended first`.
fn credit_off_memo(terms: Option<&AdvanceTerms>) -> String {
    terms.map_or_else(
        || "The advance payment is credited: the subscription ended before the month it pays for".to_string(),
        |terms| {
            format!(
                "The advance payment for {} to {} is credited: the subscription ended before that month",
                format_local_date(terms.prepaid_from),
                format_local_date(terms.prepaid_until)
            )
        },
    )
}

/// A pending item: kept while the advance may be charged, else deleted.
async fn settle_pending(
    state: &AppState,
    stored: &Stored,
    item_id: &str,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    let c = &stored.commitment;
    let company_id = c.company_id;
    if c.status != Status::AdvanceScheduled {
        return delete_item(state, company_id, item_id, "not_wanted", now, until).await;
    }
    let (_, subscription_id) = billing_ids(&state.pool, company_id).await?;
    let live = read_subscription(&advance_stripe(state), subscription_id.as_deref(), until).await?;
    let facts = charge_facts(state, company_id, live.as_ref()).await?;
    let Some(reason) = withdrawal(c, &facts) else {
        return Ok(Settled::Waiting);
    };
    let charge_at = c.terms.map(|terms| terms.charge_at);
    if reason == Withdrawal::ChargeMoved
        && charge_at.is_some_and(|at| now >= at && now < at + Duration::hours(CHARGE_LATE_HOURS))
    {
        // Stripe makes the renewal invoice shortly after the period ends.
        return Ok(Settled::Waiting);
    }
    apply(&state.pool, company_id, now, |c| {
        advance_withdrawn(c, reason, now)
    })
    .await?;
    delete_item(state, company_id, item_id, reason.as_str(), now, until).await
}

/// The invoice line of `item_id` among `lines` (every line of the invoice,
/// [`Stripe::invoice_lines`]).
fn advance_line<'a>(lines: &'a [Value], item_id: &str) -> Option<&'a Value> {
    lines.iter().find(|line| {
        stripe_id(&line["parent"]["invoice_item_details"]["invoice_item"])
            .or_else(|| stripe_id(&line["invoice_item"]))
            .is_some_and(|id| id == item_id)
    })
}

/// The total the invoice line of `item_id` charged: its amount plus the tax
/// Stripe added to it (automatic tax), so the credit returns the month's
/// tax too.
///
/// `None` without that line. `lines`: every line of the invoice (the
/// invoice object carries only the first 10).
///
/// Dahlia lists the line's tax in `taxes[]` (`tax_behavior`), older API
/// versions in `tax_amounts[]` (`inclusive`); inclusive tax is already in
/// the amount.
pub fn advance_line_total(lines: &[Value], item_id: &str) -> Option<i64> {
    let line = advance_line(lines, item_id)?;
    let added = |list: &Value, inclusive: &dyn Fn(&Value) -> bool| -> i64 {
        list.as_array().map_or(0, |taxes| {
            taxes
                .iter()
                .filter(|tax| !inclusive(tax))
                .filter_map(|tax| tax["amount"].as_i64())
                .sum()
        })
    };
    let tax = added(&line["taxes"], &|tax| tax["tax_behavior"] == "inclusive")
        + added(&line["tax_amounts"], &|tax| tax["inclusive"] == true);
    Some(line["amount"].as_i64()? + tax)
}

/// The paid invoice that carried the advance.
struct PaidAdvance<'a> {
    invoice_id: &'a str,
    /// The advance line's total (with its tax).
    amount_cents: i64,
    /// Its subscription has ended: the prepaid month never comes.
    subscription_ended: bool,
}

/// Credits the month the paid invoice carried in advance, once: a credit
/// already in the customer's balance transactions (a lost save, a replay
/// after the 24-hour key window) is recorded instead. A cancellation
/// scheduled to end before the prepaid month ends is moved to its end
/// first. When the subscription has already ended nothing is refunded
/// automatically: staff are told to review it.
async fn credit(
    state: &AppState,
    c: &Commitment,
    paid: &PaidAdvance<'_>,
    now: NaiveDateTime,
    until: Instant,
) -> Result<Settled, AdvanceError> {
    let PaidAdvance {
        invoice_id,
        amount_cents,
        ..
    } = *paid;
    let company_id = c.company_id;
    let stripe = &advance_stripe(state);
    let (Some(customer), _) = billing_ids(&state.pool, company_id).await? else {
        return Err(AdvanceError::NoCustomer);
    };
    in_time(until)?;
    let existing = stripe
        .balance_transactions(&customer)
        .await
        .map_err(|e| stripe_error(&e))?
        .into_iter()
        .find(|txn| {
            txn["metadata"]["kind"] == ADVANCE_CREDIT_KIND
                && txn["metadata"]["invoice"] == invoice_id
        });
    let new_credit = existing.is_none();
    let credit_id = if let Some(id) = existing.as_ref().and_then(stripe_id) {
        id
    } else {
        in_time(until)?;
        let created = stripe
            .credit_customer_balance(
                &customer,
                company_id,
                invoice_id,
                amount_cents,
                &credit_description(c.terms.as_ref()),
                &credit_key(company_id, invoice_id),
            )
            .await
            .map_err(|e| stripe_error(&e))?;
        stripe_id(&created).ok_or_else(|| AdvanceError::Stripe("invalid_response".into()))?
    };
    if let Some(terms) = c.terms
        && now < terms.prepaid_from
        && !paid.subscription_ended
    {
        in_time(until)?;
        cancellation::end_with_prepaid_month(state, company_id, terms.prepaid_until, invoice_id)
            .await?;
    }
    apply(&state.pool, company_id, now, |c| {
        credited(c, invoice_id, &credit_id, now)
    })
    .await?;
    tracing::info!(company_id, "onboarding advance credited back");
    if new_credit && paid.subscription_ended {
        let alert = StaffAlert::PaidAfterEnd {
            invoice_id,
            amount_cents,
        };
        notify_staff(state, company_id, c.terms.as_ref(), &alert).await;
    }
    Ok(Settled::Credited)
}

/// The id of the item behind an advance line of `invoice` (ours by the
/// line's metadata), if it has one.
///
/// `lines.data[].parent.invoice_item_details.invoice_item`, on older API
/// versions `lines.data[].invoice_item`.
pub fn advance_line_item(invoice: &Value, company_id: i32) -> Option<String> {
    invoice["lines"]["data"]
        .as_array()?
        .iter()
        .find_map(|line| {
            let ours = line["metadata"]["kind"] == ADVANCE_ITEM_KIND
                && positive_int(&line["metadata"]["company_id"]) == Some(company_id);
            ours.then(|| {
                stripe_id(&line["parent"]["invoice_item_details"]["invoice_item"])
                    .or_else(|| stripe_id(&line["invoice_item"]))
            })
            .flatten()
        })
}

/// `invoice.paid` of a subscription invoice of `company_id` (the webhook,
/// under the company lock).
///
/// An invoice carrying the advance credits its month back. The recorded item is settled (read again from Stripe); an
/// advance line whose item was never recorded (a lost save) is recorded
/// first. Any other invoice changes nothing.
pub async fn invoice_paid(
    state: &AppState,
    company_id: i32,
    invoice: &Value,
    now: NaiveDateTime,
) -> Result<(), AdvanceError> {
    let pool = &state.pool;
    let Some(stored) = store::load(pool, company_id).await? else {
        return Ok(());
    };
    let c = &stored.commitment;
    if c.advance_credit_id.is_some() {
        return Ok(());
    }
    if c.advance_item_id.is_none() {
        let Some(item_id) = advance_line_item(invoice, company_id) else {
            return Ok(());
        };
        let invoice_id = stripe_id(invoice);
        apply(pool, company_id, now, |c| {
            commitment::item_recorded(c, &item_id, invoice_id.as_deref(), now)
        })
        .await?;
    }
    let until = Instant::now() + std::time::Duration::from_secs(20);
    settle_item_locked(state, company_id, now, until).await?;
    Ok(())
}

/// The internal notice that an advance notice went out, to
/// `BILLING_ALERT_EMAIL`: company id and name, amount, charge date (no
/// other customer data). Best effort.
pub async fn notify_notice(state: &AppState, commitment: &Commitment) {
    let (Some(to), Some(terms)) = (
        crate::billing::methods::alert_email(state),
        commitment.terms,
    ) else {
        return;
    };
    let company_id = commitment.company_id;
    let name = store::company_name(&state.pool, company_id)
        .await
        .unwrap_or_default();
    let email = render_staff_notice(company_id, &name, &terms);
    if let Err(err) = cancellation::send_rendered(state, to, &email).await {
        tracing::warn!(
            company_id,
            kind = cancellation::failure_kind(&err),
            "onboarding advance staff notice failed"
        );
    }
}

fn render_staff_notice(company_id: i32, name: &str, terms: &AdvanceTerms) -> RenderedEmail {
    let amount = format_money(terms.advance_cents);
    let charge = format_local_date(terms.charge_at);
    let from = format_local_date(terms.prepaid_from);
    let lines = [
        format!("Company: {} (id {company_id})", name.trim()),
        format!("Advance payment: {amount} plus tax, with the subscription charge on {charge}"),
        format!(
            "It prepays {from} to {}. Once that charge is paid, the advance line's total (with its tax) is credited, which pays the invoice on {from}.",
            format_local_date(terms.prepaid_until)
        ),
        "An onboarding call that ends before the charge, or a cancellation request before it, avoids the advance payment."
            .to_string(),
        format!(
            "If the customer replies or emails support before {charge} to ask for a review, hold the advance for review on the superuser page."
        ),
    ];
    RenderedEmail {
        subject: format!("Onboarding advance notice sent: {}", name.trim()),
        html: wrap_html(
            lines
                .iter()
                .map(|line| html_paragraph(&escape_html(line)))
                .collect(),
        ),
        text: lines.join("\n\n"),
    }
}

/// An internal alert about an advance whose subscription ended before the
/// month it paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaffAlert<'a> {
    /// The advance line was credited off the unpaid invoice.
    CreditedOff {
        invoice_id: &'a str,
        amount_cents: Option<i64>,
    },
    /// The invoice was paid anyway: credited to the customer's balance, not
    /// refunded.
    PaidAfterEnd {
        invoice_id: &'a str,
        amount_cents: i64,
    },
}

/// Sends `alert` to `BILLING_ALERT_EMAIL` (company id and name, amounts,
/// dates; no other customer data). Best effort.
async fn notify_staff(
    state: &AppState,
    company_id: i32,
    terms: Option<&AdvanceTerms>,
    alert: &StaffAlert<'_>,
) {
    let Some(to) = crate::billing::methods::alert_email(state) else {
        return;
    };
    let name = store::company_name(&state.pool, company_id)
        .await
        .unwrap_or_default();
    let email = render_staff_alert(company_id, &name, terms, alert);
    if let Err(err) = cancellation::send_rendered(state, to, &email).await {
        tracing::warn!(
            company_id,
            kind = cancellation::failure_kind(&err),
            "onboarding advance staff alert failed"
        );
    }
}

fn render_staff_alert(
    company_id: i32,
    name: &str,
    terms: Option<&AdvanceTerms>,
    alert: &StaffAlert<'_>,
) -> RenderedEmail {
    let months = terms.map_or_else(
        || "the prepaid month".to_string(),
        |terms| {
            format!(
                "{} to {}",
                format_local_date(terms.prepaid_from),
                format_local_date(terms.prepaid_until)
            )
        },
    );
    let company = format!("Company: {} (id {company_id})", name.trim());
    let (subject, lines) = match *alert {
        StaffAlert::CreditedOff {
            invoice_id,
            amount_cents,
        } => (
            format!("Onboarding advance credited off: {}", name.trim()),
            vec![
                company,
                format!(
                    "The subscription ended while the advance payment for {months} was unpaid on invoice {invoice_id}."
                ),
                format!(
                    "A credit note of {} took the advance line (with its tax) off that invoice. The rest of the invoice is still due.",
                    amount_cents.map_or_else(|| "the advance line".to_string(), format_money)
                ),
            ],
        ),
        StaffAlert::PaidAfterEnd {
            invoice_id,
            amount_cents,
        } => (
            format!(
                "Onboarding advance paid after the subscription ended: {}",
                name.trim()
            ),
            vec![
                company,
                format!(
                    "Invoice {invoice_id} with the advance payment for {months} was paid, but the subscription has ended, so that month never comes."
                ),
                format!(
                    "{} (with tax) was credited to the customer's Stripe balance. Nothing was refunded: review whether to refund it.",
                    format_money(amount_cents)
                ),
            ],
        ),
    };
    RenderedEmail {
        subject,
        html: wrap_html(
            lines
                .iter()
                .map(|line| html_paragraph(&escape_html(line)))
                .collect(),
        ),
        text: lines.join("\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn the_deadline_key_is_the_local_day_the_consent_named() {
        let end_of_nov_22 =
            NaiveDateTime::parse_from_str("2026-11-23 04:59:59", "%Y-%m-%d %H:%M:%S").unwrap();
        assert_eq!(deadline_key(end_of_nov_22), "2026-11-22");
    }

    #[test]
    fn an_item_after_a_withdrawn_one_gets_a_new_creation_key() {
        assert_eq!(
            item_key(7, "2026-11-22", 0),
            "onboarding-advance:7:2026-11-22"
        );
        assert_eq!(
            item_key(7, "2026-11-22", 1),
            "onboarding-advance:7:2026-11-22:1"
        );
        assert_eq!(credit_key(7, "in_1"), "onboarding-advance:7:in_1:credit");
    }

    #[test]
    fn an_item_is_found_again_by_company_and_deadline_unless_deleted() {
        let item = |deadline: &str, company: &str| {
            json!({ "id": "ii_1", "metadata": {
                "kind": "onboarding_advance", "deadline": deadline, "company_id": company } })
        };
        assert!(is_advance_item(&item("2026-11-20", "7"), 7, "2026-11-20"));
        assert!(!is_advance_item(&item("2026-11-21", "7"), 7, "2026-11-20"));
        assert!(!is_advance_item(&item("2026-11-20", "8"), 7, "2026-11-20"));
        let mut deleted = item("2026-11-20", "7");
        deleted["deleted"] = json!(true);
        assert!(!is_advance_item(&deleted, 7, "2026-11-20"));
    }

    #[test]
    fn the_advance_line_is_found_by_its_metadata_on_either_api_shape() {
        let dahlia = json!({ "lines": { "data": [
            { "id": "il_1", "metadata": {} },
            { "id": "il_2", "metadata": { "kind": "onboarding_advance", "company_id": "7" },
              "parent": { "type": "invoice_item_details",
                          "invoice_item_details": { "invoice_item": "ii_9" } } },
        ] } });
        assert_eq!(advance_line_item(&dahlia, 7).as_deref(), Some("ii_9"));
        assert_eq!(advance_line_item(&dahlia, 8), None);
        let older = json!({ "lines": { "data": [
            { "metadata": { "kind": "onboarding_advance", "company_id": "7" }, "invoice_item": "ii_3" },
        ] } });
        assert_eq!(advance_line_item(&older, 7).as_deref(), Some("ii_3"));
        assert_eq!(advance_line_item(&json!({}), 7), None);
    }

    #[test]
    fn the_live_subscription_gives_its_monthly_total_next_charge_and_anchor() {
        let trial_end = at("2027-01-28 15:00").and_utc().timestamp();
        let live = live_subscription(&json!({
            "id": "sub_1", "status": "trialing", "trial_end": trial_end,
            "billing_cycle_anchor": trial_end, "cancel_at_period_end": false,
            "items": { "data": [
                { "id": "si_1", "quantity": 1, "current_period_end": trial_end,
                  "price": { "id": "p1", "lookup_key": "granite_base_monthly_v1", "unit_amount": 30_000 } },
                { "id": "si_2", "quantity": 2, "current_period_end": trial_end,
                  "price": { "id": "p2", "lookup_key": "granite_extra_user_monthly_v1", "unit_amount": 3_000 } },
            ] },
        }));
        assert!(live.live && !live.cancel_scheduled);
        assert_eq!(live.monthly_cents, 36_000);
        assert_eq!(live.next_charge_at, Some(at("2027-01-28 15:00")));
        assert_eq!(live.anchor, Some(at("2027-01-28 15:00")));
    }

    #[test]
    fn the_staff_notice_names_company_amount_and_dates_only() {
        let terms = AdvanceTerms {
            advance_cents: 30_000,
            charge_at: at("2027-01-28 15:00"),
            prepaid_from: at("2027-02-28 15:00"),
            prepaid_until: at("2027-03-28 15:00"),
        };
        let email = render_staff_notice(7, "Rock & Co", &terms);
        assert!(email.text.contains("Company: Rock & Co (id 7)"));
        assert_eq!(
            email.text,
            "Company: Rock & Co (id 7)\n\n\
             Advance payment: $300.00 plus tax, with the subscription charge on January 28, 2027\n\n\
             It prepays February 28, 2027 to March 28, 2027. Once that charge is paid, the advance line's total (with its tax) is credited, which pays the invoice on February 28, 2027.\n\n\
             An onboarding call that ends before the charge, or a cancellation request before it, avoids the advance payment.\n\n\
             If the customer replies or emails support before January 28, 2027 to ask for a review, hold the advance for review on the superuser page."
        );
        assert!(email.html.contains("Rock &amp; Co"));
        assert!(!email.text.contains('\u{2014}'));
        assert!(!email.text.to_lowercase().contains("fee"));
        assert_eq!(
            item_description(&terms),
            "Advance payment for February 28, 2027 to March 28, 2027"
        );
        assert_eq!(
            credit_description(Some(&terms)),
            "Credit for the advance payment of February 28, 2027 to March 28, 2027"
        );
    }

    #[test]
    fn the_credit_is_the_advance_line_with_its_exclusive_tax() {
        let lines = json!([
            { "amount": 30_000, "taxes": [{ "amount": 2_100, "tax_behavior": "exclusive" }] },
            { "amount": 30_000, "parent": { "invoice_item_details": { "invoice_item": "ii_9" } },
              "taxes": [
                { "amount": 2_100, "tax_behavior": "exclusive" },
                { "amount": 300, "tax_behavior": "exclusive" },
              ] },
            { "amount": 5_000, "invoice_item": "ii_old",
              "tax_amounts": [{ "amount": 350, "inclusive": false }, { "amount": 20, "inclusive": true }] },
            { "amount": 4_000, "parent": { "invoice_item_details": { "invoice_item": "ii_incl" } },
              "taxes": [{ "amount": 280, "tax_behavior": "inclusive" }] },
        ]);
        let lines = lines.as_array().unwrap();
        assert_eq!(advance_line_total(lines, "ii_9"), Some(32_400));
        assert_eq!(advance_line_total(lines, "ii_old"), Some(5_350));
        assert_eq!(advance_line_total(lines, "ii_incl"), Some(4_000));
        assert_eq!(advance_line_total(lines, "ii_none"), None);
    }

    #[test]
    fn the_advance_is_priced_on_the_base_items_product() {
        let live = live_subscription(&json!({
            "id": "sub_1", "status": "active",
            "items": { "data": [
                { "id": "si_2", "quantity": 2,
                  "price": { "id": "p2", "lookup_key": "granite_extra_user_monthly_v1",
                             "unit_amount": 3_000, "product": "prod_seat" } },
                { "id": "si_1", "quantity": 1,
                  "price": { "id": "p1", "lookup_key": "granite_base_monthly_v1",
                             "unit_amount": 30_000, "product": { "id": "prod_base" } } },
            ] },
        }));
        assert_eq!(live.base_product.as_deref(), Some("prod_base"));
        assert_eq!(
            live_subscription(&json!({ "id": "sub_1" })).base_product,
            None
        );
    }

    #[test]
    fn the_staff_alerts_name_company_invoice_and_amounts_only() {
        let terms = AdvanceTerms {
            advance_cents: 30_000,
            charge_at: at("2027-01-28 15:00"),
            prepaid_from: at("2027-02-28 15:00"),
            prepaid_until: at("2027-03-28 15:00"),
        };
        let off = render_staff_alert(
            7,
            "Rock & Co",
            Some(&terms),
            &StaffAlert::CreditedOff {
                invoice_id: "in_1",
                amount_cents: Some(32_100),
            },
        );
        assert_eq!(off.subject, "Onboarding advance credited off: Rock & Co");
        assert!(off.text.contains("Company: Rock & Co (id 7)"));
        assert!(
            off.text
                .contains("February 28, 2027 to March 28, 2027 was unpaid on invoice in_1")
        );
        assert!(off.text.contains("A credit note of $321.00"));
        let paid = render_staff_alert(
            7,
            "Rock & Co",
            None,
            &StaffAlert::PaidAfterEnd {
                invoice_id: "in_1",
                amount_cents: 32_100,
            },
        );
        assert!(
            paid.text
                .contains("Nothing was refunded: review whether to refund it.")
        );
        for email in [off, paid] {
            assert!(!email.text.contains('\u{2014}'));
            assert!(email.html.contains("Rock &amp; Co"));
        }
    }
}
