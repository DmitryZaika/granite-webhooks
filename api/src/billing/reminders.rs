//! Billing reminder emails to a company's managers.
//!
//! A dunning cycle is either a failed renewal (starts at `failed_payment_at`)
//! or a company asked to pay that has not subscribed yet (starts at
//! `company.billing_required_at`). Both run the same countdown in UTC: day 0,
//! day 4 (three days left), day 6 (paused tomorrow), the pause on day 7 once
//! the access gate really pauses, then one email when the cycle is settled.
//! A `first_charge` cycle (starts at `company_billing.trial_end_at`) sends one
//! email 7 days before the first charge of a self-serve subscription. Port of
//! the CRM's `billingReminders.ts`.
// Generic over the store, like `machine`.
#![allow(clippy::future_not_send)]

use super::CompanyBilling;
use super::account::Card;
use super::reminder_emails::{
    FirstChargeEmailInput, ReminderEmailInput, render_first_charge_email, render_reminder_email,
};
use super::store::StoreResult;
use super::{PaymentStatus, add_grace_days, first_payment_deadline, is_awaiting_first_payment};
use crate::webhooks_client::SendFailure;
use chrono::{Duration, NaiveDateTime};
use serde_json::Value;
use std::collections::HashSet;

pub const REMINDER_MAX_ATTEMPTS: i32 = 3;
/// Cycles that started longer ago get no emails, so a deploy does not email
/// companies paused long ago out of the blue.
pub const REMINDER_CYCLE_MAX_AGE_DAYS: i64 = 14;
/// How long after a failed payment recovered "payment received" may go out.
pub const RECOVERED_NOTICE_WINDOW_DAYS: i64 = 2;
/// A claim still `sending` after this crashed between send and record.
pub const STUCK_SENDING_MINUTES: i64 = 15;
pub const REMINDER_EMAILS_PER_TICK: usize = 20;
/// No new company is started after this, so a tick ends well inside the
/// one-minute schedule and the Lambda timeout.
pub const REMINDER_TICK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(40);
/// The first-charge email goes out this many days before `trial_end_at`.
pub const FIRST_CHARGE_NOTICE_DAYS: i64 = 7;
/// A first-charge cycle Stripe says needs no email is asked again after this.
pub const FIRST_CHARGE_RECHECK_HOURS: i64 = 6;
/// A refused send is tried again once its last attempt is this old.
pub const REMINDER_RETRY_AFTER_MINUTES: i64 = 5;
/// `billing_reminder_emails.user_id` of a company-level `skipped` row.
pub const NO_USER: i32 = 0;

/// Why a company is being reminded (`billing_reminder_emails.cycle_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DunningKind {
    /// A renewal payment failed.
    PaymentFailed,
    /// Asked to pay and has not subscribed yet.
    FirstPayment,
    /// The first charge after a self-serve free period is coming.
    FirstCharge,
}

impl DunningKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaymentFailed => "payment_failed",
            Self::FirstPayment => "first_payment",
            Self::FirstCharge => "first_charge",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "payment_failed" => Some(Self::PaymentFailed),
            "first_payment" => Some(Self::FirstPayment),
            "first_charge" => Some(Self::FirstCharge),
            _ => None,
        }
    }
}

/// Day 0, day 4, day 6, the pause on day 7, the email after it is settled
/// (payment received, or subscribed), and the first-charge notice
/// (`billing_reminder_emails.step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReminderStep {
    Started,
    ThreeDaysLeft,
    PauseTomorrow,
    Paused,
    Resolved,
    /// `first_charge` only: 7 days before the charge.
    Upcoming,
}

impl ReminderStep {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::ThreeDaysLeft => "three_days_left",
            Self::PauseTomorrow => "pause_tomorrow",
            Self::Paused => "paused",
            Self::Resolved => "resolved",
            Self::Upcoming => "upcoming",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(Self::Started),
            "three_days_left" => Some(Self::ThreeDaysLeft),
            "pause_tomorrow" => Some(Self::PauseTomorrow),
            "paused" => Some(Self::Paused),
            "resolved" => Some(Self::Resolved),
            "upcoming" => Some(Self::Upcoming),
            _ => None,
        }
    }
}

/// `billing_reminder_emails.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderStatus {
    Sending,
    Sent,
    /// SES refused it: may be retried while attempts are left.
    Failed,
    /// It may or may not have gone out: never resent.
    Unknown,
    /// Company-level marker (`NO_USER`): Stripe said no email is due for
    /// this cycle; asked again after `FIRST_CHARGE_RECHECK_HOURS`.
    Skipped,
}

impl ReminderStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Skipped => "skipped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sending" => Some(Self::Sending),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DunningCycle {
    pub kind: DunningKind,
    pub started_at: NaiveDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DueReminder {
    pub cycle: DunningCycle,
    pub step: ReminderStep,
    /// When access is (or was) paused, or the first charge is due; `None`
    /// once the cycle is settled.
    pub pauses_at: Option<NaiveDateTime>,
}

/// What the processor reads about a company, uncached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanyBillingSnapshot {
    /// `company.billing_required`.
    pub required: bool,
    /// When Stripe payment was last turned on (`company.billing_required_at`).
    pub required_since: Option<NaiveDateTime>,
    pub record: Option<CompanyBilling>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderKey {
    pub company_id: i32,
    pub user_id: i32,
    pub cycle: DunningCycle,
    pub step: ReminderStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderRow {
    pub id: i64,
    pub key: ReminderKey,
    pub status: ReminderStatus,
    pub attempts: i32,
    /// When the last attempt was claimed.
    pub claimed_at: NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderContact {
    pub user_id: i32,
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimedReminder {
    pub id: i64,
    pub company_id: i32,
}

/// One line of the first charge (base plan, extra users, an add-on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeLine {
    pub label: String,
    pub amount_cents: i64,
}

/// The first charge of a trialing subscription, as Stripe has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstChargeQuote {
    /// When Stripe charges it (the subscription's `trial_end`).
    pub charge_at: NaiveDateTime,
    /// Before sales tax.
    pub amount_cents: i64,
    pub lines: Vec<ChargeLine>,
    /// `Visa ending in 4242`, when Stripe knows the default method.
    pub payment_method: Option<String>,
}

/// A reminder ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderEmail {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderSendResult {
    Sent {
        message_id: Option<String>,
    },
    /// SES answered and did not take the email: safe to try again.
    Rejected {
        code: String,
    },
    /// It may or may not have gone out: never resent.
    Unknown {
        code: String,
    },
}

/// Why one company was skipped this tick.
#[derive(Debug, thiserror::Error)]
pub enum ReminderError {
    /// Another request holds the company's billing lock: the next tick
    /// picks the company up again.
    #[error("billing is busy")]
    Busy,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("stripe: {0}")]
    Stripe(String),
}

/// Ids, kind, step and error codes for a report: never addresses or names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportContext {
    pub company_id: Option<i32>,
    pub reminder_id: Option<i64>,
    pub user_id: Option<i32>,
    pub kind: Option<DunningKind>,
    pub step: Option<ReminderStep>,
    pub error_code: Option<String>,
}

/// Where reminders and the billing facts they need live.
#[allow(async_fn_in_trait)] // used with concrete types only
pub trait ReminderStore {
    /// Companies that may have a dunning reminder due: billing required, and
    /// an open cycle or a recently settled one (`LIMIT 500`).
    async fn candidate_company_ids(&self, now: NaiveDateTime) -> StoreResult<Vec<i32>>;
    /// Companies whose trialing subscription's `trial_end_at` is after `now`
    /// and at most `FIRST_CHARGE_NOTICE_DAYS` away, not cancelled by then,
    /// and without a `skipped` row for that cycle newer than
    /// `FIRST_CHARGE_RECHECK_HOURS` (`LIMIT 500`).
    async fn first_charge_company_ids(&self, now: NaiveDateTime) -> StoreResult<Vec<i32>>;
    async fn snapshot(&self, company_id: i32) -> StoreResult<CompanyBillingSnapshot>;
    /// Non-deleted users with the Manager position in the company, by id.
    async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>>;
    /// Non-deleted users of the company among `user_ids` with a non-empty
    /// (trimmed) email.
    async fn contacts(
        &self,
        company_id: i32,
        user_ids: &[i32],
    ) -> StoreResult<Vec<ReminderContact>>;
    async fn company_name(&self, company_id: i32) -> StoreResult<String>;
    async fn reminders(
        &self,
        company_id: i32,
        cycle: DunningCycle,
    ) -> StoreResult<Vec<ReminderRow>>;
    /// Inserts the row as `sending` (or re-takes a `failed` one with attempts
    /// left, last claimed `REMINDER_RETRY_AFTER_MINUTES` ago). The unique key
    /// makes this the only way to send: `None` means another run owns it, it
    /// is done, or its retry is not due yet.
    async fn claim(&self, key: &ReminderKey, now: NaiveDateTime) -> StoreResult<Option<i64>>;
    /// Records (or re-stamps) the company-level `skipped` row of a cycle.
    async fn skip_cycle(
        &self,
        company_id: i32,
        cycle: DunningCycle,
        step: ReminderStep,
        now: NaiveDateTime,
    ) -> StoreResult<()>;
    async fn mark_sent(
        &self,
        row: ClaimedReminder,
        message_id: Option<&str>,
        now: NaiveDateTime,
    ) -> StoreResult<()>;
    async fn mark_failed(&self, row: ClaimedReminder, error_code: &str) -> StoreResult<()>;
    async fn mark_unknown(&self, row: ClaimedReminder, error_code: &str) -> StoreResult<()>;
    /// Flips `sending` rows claimed before `cutoff` to `unknown` (error code
    /// `stuck_sending`) and returns them (`LIMIT 100`).
    async fn sweep_stuck(&self, cutoff: NaiveDateTime) -> StoreResult<Vec<ReminderRow>>;
}

/// The lock, Stripe, email and reporting around the store.
#[allow(async_fn_in_trait)] // used with concrete types only
pub trait ReminderEffects {
    /// A held company billing lock.
    type Lock;
    /// `Err(ReminderError::Busy)` when another request holds it.
    async fn lock(&self, company_id: i32) -> Result<Self::Lock, ReminderError>;
    async fn unlock(&self, lock: Self::Lock);
    /// The access gate's pause decision for one company (`settle_locked`):
    /// may pause, restore or pick up a payment from Stripe. Runs under the
    /// lock.
    async fn settle(
        &self,
        company_id: i32,
        required_since: Option<NaiveDateTime>,
        now: NaiveDateTime,
    ) -> Result<Option<CompanyBilling>, ReminderError>;
    /// The first charge of the company's trialing subscription; `None` when
    /// Stripe has no such subscription.
    async fn first_charge_quote(
        &self,
        record: &CompanyBilling,
    ) -> Result<Option<FirstChargeQuote>, ReminderError>;
    /// Sends one email within the send timeout (the lock may be held).
    async fn send(&self, email: &ReminderEmail) -> ReminderSendResult;
    /// Logs a problem; `Busy` is a warning only, the rest also go to `PostHog`.
    async fn report(&self, problem: &str, context: &ReportContext);
    /// Time since the tick started, for the deadline.
    fn elapsed(&self) -> std::time::Duration;
    /// The app's public origin, for the one link in each email.
    fn app_url(&self) -> &str;
}

/// What a tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReminderTickResult {
    /// Reminders claimed (each then sent, failed or unknown).
    pub processed: usize,
    pub sent: usize,
    /// SES refused (retried on a later tick, up to 3 attempts).
    pub failed: usize,
    /// Outcome unknown: never resent.
    pub unknown: usize,
    /// `sending` claims older than 15 minutes, now `unknown`.
    pub stuck: usize,
    /// Companies skipped after an error (busy lock, database, Stripe).
    pub company_errors: usize,
}

/// How much one tick may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderLimits {
    pub max_emails: usize,
    pub deadline: std::time::Duration,
}

impl Default for ReminderLimits {
    fn default() -> Self {
        Self {
            max_emails: REMINDER_EMAILS_PER_TICK,
            deadline: REMINDER_TICK_DEADLINE,
        }
    }
}

fn is_fresh(started_at: NaiveDateTime, now: NaiveDateTime) -> bool {
    now - started_at < Duration::days(REMINDER_CYCLE_MAX_AGE_DAYS)
}

/// Day 0, 4 or 6 of a countdown to `pauses_at`: "3 days left" starts three
/// days before the pause and "tomorrow" one day before. `None` once it has
/// passed.
fn countdown_step(pauses_at: NaiveDateTime, now: NaiveDateTime) -> Option<ReminderStep> {
    let left = pauses_at - now;
    if left <= Duration::zero() {
        None
    } else if left <= Duration::days(1) {
        Some(ReminderStep::PauseTomorrow)
    } else if left <= Duration::days(3) {
        Some(ReminderStep::ThreeDaysLeft)
    } else {
        Some(ReminderStep::Started)
    }
}

fn failed_payment_reminder(
    record: &CompanyBilling,
    failed_at: NaiveDateTime,
    now: NaiveDateTime,
) -> Option<DueReminder> {
    if !is_fresh(failed_at, now) {
        return None;
    }
    let cycle = DunningCycle {
        kind: DunningKind::PaymentFailed,
        started_at: failed_at,
    };
    // Counted back from the stored grace end, so a cycle opened under an older
    // grace length still gets true dates.
    let pauses_at = record
        .grace_period_ends_at
        .unwrap_or_else(|| add_grace_days(failed_at));
    let due = |step| DueReminder {
        cycle,
        step,
        pauses_at: Some(pauses_at),
    };
    if record.payment_status == PaymentStatus::Suspended {
        return Some(due(ReminderStep::Paused));
    }
    // Past the grace end but not paused yet (Stripe has not confirmed the
    // non-payment): nothing true to say until the gate pauses it.
    countdown_step(pauses_at, now).map(due)
}

fn first_payment_reminder(
    required_since: NaiveDateTime,
    now: NaiveDateTime,
) -> Option<DueReminder> {
    if !is_fresh(required_since, now) {
        return None;
    }
    let pauses_at = first_payment_deadline(required_since);
    // The gate pauses on time once Stripe shows no payment, and settle has just
    // asked Stripe, so the window ending is the pause.
    Some(DueReminder {
        cycle: DunningCycle {
            kind: DunningKind::FirstPayment,
            started_at: required_since,
        },
        step: countdown_step(pauses_at, now).unwrap_or(ReminderStep::Paused),
        pauses_at: Some(pauses_at),
    })
}

/// The one reminder that is due for a company right now, if any. Only the
/// latest step counts: a cycle first seen on day 6 gets "tomorrow" alone.
pub fn due_reminder(snapshot: &CompanyBillingSnapshot, now: NaiveDateTime) -> Option<DueReminder> {
    let record = snapshot.record.as_ref();
    let required_since = snapshot.required_since;
    if is_awaiting_first_payment(record) {
        return required_since.and_then(|since| first_payment_reminder(since, now));
    }
    let record = record?;
    if let Some(failed_at) = record.failed_payment_at
        && matches!(
            record.payment_status,
            PaymentStatus::PastDue | PaymentStatus::Suspended
        )
    {
        return failed_payment_reminder(record, failed_at, now);
    }
    if record.payment_status != PaymentStatus::Active {
        return None;
    }
    if let (Some(failed_at), Some(recovered_at)) = (record.recovered_failed_at, record.recovered_at)
        && now - recovered_at < Duration::days(RECOVERED_NOTICE_WINDOW_DAYS)
    {
        return Some(DueReminder {
            cycle: DunningCycle {
                kind: DunningKind::PaymentFailed,
                started_at: failed_at,
            },
            step: ReminderStep::Resolved,
            pauses_at: None,
        });
    }
    // Subscribed after being asked to: settles the first-payment cycle (sent
    // only when that cycle was emailed about, see `plan_reminder`).
    required_since
        .filter(|since| is_fresh(*since, now))
        .map(|since| DueReminder {
            cycle: DunningCycle {
                kind: DunningKind::FirstPayment,
                started_at: since,
            },
            step: ReminderStep::Resolved,
            pauses_at: None,
        })
}

/// What may be due once the company is settled: a grace period that ended
/// without a confirmed pause counts as "paused", so the processor still takes
/// the lock and lets the gate decide. Only `due_reminder` decides what is sent.
fn prospective_reminder(
    snapshot: &CompanyBillingSnapshot,
    now: NaiveDateTime,
) -> Option<DueReminder> {
    let due = due_reminder(snapshot, now);
    let Some(record) = snapshot.record.as_ref() else {
        return due;
    };
    let failed_at = record.failed_payment_at;
    let (None, PaymentStatus::PastDue, Some(failed_at)) = (due, record.payment_status, failed_at)
    else {
        return due;
    };
    if !is_fresh(failed_at, now) {
        return None;
    }
    let pauses_at = record
        .grace_period_ends_at
        .unwrap_or_else(|| add_grace_days(failed_at));
    if now < pauses_at {
        return None;
    }
    Some(DueReminder {
        cycle: DunningCycle {
            kind: DunningKind::PaymentFailed,
            started_at: failed_at,
        },
        step: ReminderStep::Paused,
        pauses_at: Some(pauses_at),
    })
}

/// The first-charge notice for a charge at `charge_at`, when it is inside
/// the notice window. A new charge date is a new cycle.
fn first_charge_notice(charge_at: NaiveDateTime, now: NaiveDateTime) -> Option<DueReminder> {
    if now >= charge_at || charge_at - now > Duration::days(FIRST_CHARGE_NOTICE_DAYS) {
        return None;
    }
    Some(DueReminder {
        cycle: DunningCycle {
            kind: DunningKind::FirstCharge,
            started_at: charge_at,
        },
        step: ReminderStep::Upcoming,
        pauses_at: Some(charge_at),
    })
}

/// A self-serve subscription whose free period ends within the notice window
/// and is not cancelled by then.
///
/// Stripe charges whether or not the company is flagged `billing_required`,
/// so only the record decides.
pub fn first_charge_due(record: &CompanyBilling, now: NaiveDateTime) -> Option<DueReminder> {
    let trial_end = record.trial_end_at?;
    if record.subscription_status != "trialing"
        || record
            .cancel_at
            .is_some_and(|cancel_at| cancel_at <= trial_end)
    {
        return None;
    }
    first_charge_notice(trial_end, now)
}

/// No row yet, or a refused one with attempts left whose last try is
/// `REMINDER_RETRY_AFTER_MINUTES` old.
fn can_claim(row: Option<&ReminderRow>, now: NaiveDateTime) -> bool {
    row.is_none_or(|row| {
        row.status == ReminderStatus::Failed
            && row.attempts < REMINDER_MAX_ATTEMPTS
            && now - row.claimed_at >= Duration::minutes(REMINDER_RETRY_AFTER_MINUTES)
    })
}

fn emailed_about_cycle(rows: &[ReminderRow]) -> bool {
    rows.iter().any(|row| {
        row.key.step != ReminderStep::Resolved
            && matches!(row.status, ReminderStatus::Sent | ReminderStatus::Unknown)
    })
}

/// One email per mailbox: two manager accounts sharing an address get one
/// copy, always through the same (lowest) user id so the choice is stable.
fn unique_by_email(mut contacts: Vec<ReminderContact>) -> Vec<ReminderContact> {
    contacts.sort_by_key(|contact| contact.user_id);
    let mut seen = HashSet::new();
    contacts.retain(|contact| seen.insert(contact.email.trim().to_lowercase()));
    contacts
}

/// The managers of a company, one per mailbox.
async fn recipients<S: ReminderStore>(
    store: &S,
    company_id: i32,
) -> Result<Vec<ReminderContact>, ReminderError> {
    let ids = store.managers(company_id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(unique_by_email(store.contacts(company_id, &ids).await?))
}

/// The contacts who still need the step (see `can_claim`).
fn open_contacts(
    contacts: Vec<ReminderContact>,
    rows: &[ReminderRow],
    step: ReminderStep,
    now: NaiveDateTime,
) -> Vec<ReminderContact> {
    contacts
        .into_iter()
        .filter(|contact| {
            can_claim(
                rows.iter()
                    .find(|row| row.key.user_id == contact.user_id && row.key.step == step),
                now,
            )
        })
        .collect()
}

struct ReminderPlan {
    due: DueReminder,
    contacts: Vec<ReminderContact>,
}

/// The due step and the managers who still need it.
async fn plan_reminder<S: ReminderStore>(
    store: &S,
    company_id: i32,
    snapshot: &CompanyBillingSnapshot,
    now: NaiveDateTime,
    prospective: bool,
) -> Result<Option<ReminderPlan>, ReminderError> {
    let due = if prospective {
        prospective_reminder(snapshot, now)
    } else {
        due_reminder(snapshot, now)
    };
    let Some(due) = due else {
        return Ok(None);
    };
    let rows = store.reminders(company_id, due.cycle).await?;
    // The closing email only follows a cycle we actually emailed about.
    if due.step == ReminderStep::Resolved && !emailed_about_cycle(&rows) {
        return Ok(None);
    }
    let contacts = open_contacts(recipients(store, company_id).await?, &rows, due.step, now);
    Ok((!contacts.is_empty()).then_some(ReminderPlan { due, contacts }))
}

const REJECTED_CODE: &str = "rejected";
const SEND_FAILED_CODE: &str = "send_failed";

/// Only a refusal the webhooks Lambda answered with proves the email did not
/// go out; anything else (5xx, network, timeout) may have. The failure's text
/// can hold an address, so only a fixed code is kept.
pub fn classify_send_failure(failure: &SendFailure) -> ReminderSendResult {
    match failure {
        SendFailure::Rejected(_) => ReminderSendResult::Rejected {
            code: REJECTED_CODE.into(),
        },
        SendFailure::Failed(_) => ReminderSendResult::Unknown {
            code: SEND_FAILED_CODE.into(),
        },
    }
}

fn charge_line_label(price: &Value, quantity: i64) -> String {
    let metadata = &price["metadata"];
    match (metadata["kind"].as_str(), metadata["addon_key"].as_str()) {
        (Some("base"), _) => "Base plan, up to 10 users".into(),
        (Some("seat"), _) if quantity == 1 => "1 active user above 10".into(),
        (Some("seat"), _) => format!("{quantity} active users above 10"),
        (_, Some("signatures")) => "Signatures add-on".into(),
        (_, Some("quotes")) => "Quotes add-on".into(),
        (_, Some("quickbooks")) => "QuickBooks add-on".into(),
        _ => "Subscription".into(),
    }
}

/// A trialing subscription set to cancel by the end of its trial is never
/// charged.
fn ends_before_first_charge(subscription: &Value) -> bool {
    if subscription["cancel_at_period_end"].as_bool() == Some(true) {
        return true;
    }
    match (
        subscription["cancel_at"].as_i64(),
        subscription["trial_end"].as_i64(),
    ) {
        (Some(cancel_at), Some(trial_end)) => cancel_at <= trial_end,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The first charge of a trialing Stripe subscription object, on its
/// `trial_end`.
///
/// `None` when it is not trialing, cancels before the charge, or has no
/// priced item. Discounts are not applied (v1 has no coupons).
pub fn first_charge_quote_from(
    subscription: &Value,
    payment_method: Option<String>,
) -> Option<FirstChargeQuote> {
    if subscription["status"].as_str() != Some("trialing") || ends_before_first_charge(subscription)
    {
        return None;
    }
    let charge_at =
        chrono::DateTime::from_timestamp(subscription["trial_end"].as_i64()?, 0)?.naive_utc();
    let lines: Vec<ChargeLine> = subscription["items"]["data"]
        .as_array()?
        .iter()
        .filter_map(|item| {
            let quantity = item["quantity"].as_i64().unwrap_or(1);
            let price = &item["price"];
            let amount_cents = price["unit_amount"].as_i64()?.checked_mul(quantity)?;
            (amount_cents > 0).then(|| ChargeLine {
                label: charge_line_label(price, quantity),
                amount_cents,
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(FirstChargeQuote {
        charge_at,
        amount_cents: lines.iter().map(|line| line.amount_cents).sum(),
        lines,
        payment_method,
    })
}

/// `Visa ending in 4242`.
pub fn describe_card(card: &Card) -> String {
    let brand = match card.brand.as_str() {
        "" => "Card".to_string(),
        "amex" => "American Express".to_string(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    };
    format!("{brand} ending in {}", card.last4)
}

const fn error_code_of(error: &ReminderError) -> &'static str {
    match error {
        ReminderError::Busy => "busy",
        ReminderError::Database(_) => "database",
        ReminderError::Stripe(_) => "stripe",
    }
}

fn limit_reached<E: ReminderEffects>(
    effects: &E,
    limits: ReminderLimits,
    result: &ReminderTickResult,
) -> bool {
    result.processed >= limits.max_emails || effects.elapsed() >= limits.deadline
}

/// Records the outcome of one send; a failed `mark_*` is the caller's error.
async fn send_claimed<S: ReminderStore, E: ReminderEffects>(
    store: &S,
    effects: &E,
    claimed: ClaimedReminder,
    email: &ReminderEmail,
    context: &ReportContext,
    now: NaiveDateTime,
    result: &mut ReminderTickResult,
) -> Result<(), ReminderError> {
    match effects.send(email).await {
        ReminderSendResult::Sent { message_id } => {
            // A failure here leaves the row `sending`; the sweep then reports it.
            store.mark_sent(claimed, message_id.as_deref(), now).await?;
            result.sent += 1;
        }
        ReminderSendResult::Rejected { code } => {
            store.mark_failed(claimed, &code).await?;
            result.failed += 1;
            let context = ReportContext {
                error_code: Some(code),
                ..context.clone()
            };
            effects
                .report("Billing reminder email was rejected", &context)
                .await;
        }
        ReminderSendResult::Unknown { code } => {
            store.mark_unknown(claimed, &code).await?;
            result.unknown += 1;
            let context = ReportContext {
                error_code: Some(code),
                ..context.clone()
            };
            effects
                .report(
                    "Billing reminder email outcome unknown, not resent",
                    &context,
                )
                .await;
        }
    }
    Ok(())
}

const fn send_context(
    claimed: ClaimedReminder,
    user_id: i32,
    cycle: DunningCycle,
    step: ReminderStep,
) -> ReportContext {
    ReportContext {
        company_id: Some(claimed.company_id),
        reminder_id: Some(claimed.id),
        user_id: Some(user_id),
        kind: Some(cycle.kind),
        step: Some(step),
        error_code: None,
    }
}

/// Re-checks the company and sends at most one email, all inside one hold of
/// the company lock. One email per hold keeps the lock short: Stripe webhooks
/// and a manager paying wait on the same lock. `false` when nothing is left
/// to send for this company.
async fn send_next_locked<S: ReminderStore, E: ReminderEffects>(
    store: &S,
    effects: &E,
    company_id: i32,
    now: NaiveDateTime,
    result: &mut ReminderTickResult,
) -> Result<bool, ReminderError> {
    let snapshot = store.snapshot(company_id).await?;
    if !snapshot.required {
        return Ok(false);
    }
    // The same decision as a page load: may pause, restore, or find a payment
    // in Stripe before the step is chosen.
    let record = effects
        .settle(company_id, snapshot.required_since, now)
        .await?;
    let snapshot = CompanyBillingSnapshot { record, ..snapshot };
    let Some(plan) = plan_reminder(store, company_id, &snapshot, now, false).await? else {
        return Ok(false);
    };
    let DueReminder {
        cycle,
        step,
        pauses_at,
    } = plan.due;
    let company_name = store.company_name(company_id).await?;
    for contact in plan.contacts {
        let Some(rendered) = render_reminder_email(&ReminderEmailInput {
            kind: cycle.kind,
            step,
            company_name: &company_name,
            user_name: contact.name.as_deref(),
            pauses_at,
            app_url: effects.app_url(),
        }) else {
            continue;
        };
        let key = ReminderKey {
            company_id,
            user_id: contact.user_id,
            cycle,
            step,
        };
        let Some(id) = store.claim(&key, now).await? else {
            continue;
        };
        result.processed += 1;
        let claimed = ClaimedReminder { id, company_id };
        let email = ReminderEmail {
            to: contact.email,
            subject: rendered.subject,
            html: rendered.html,
            text: rendered.text,
        };
        let context = send_context(claimed, contact.user_id, cycle, step);
        send_claimed(store, effects, claimed, &email, &context, now, result).await?;
        return Ok(true);
    }
    Ok(false)
}

/// One company's dunning reminders, one locked email at a time.
async fn dunning_company<S: ReminderStore, E: ReminderEffects>(
    store: &S,
    effects: &E,
    company_id: i32,
    now: NaiveDateTime,
    limits: ReminderLimits,
    result: &mut ReminderTickResult,
) -> Result<(), ReminderError> {
    // Cheap check first, without the lock or Stripe: most ticks find every
    // manager already emailed for the current step.
    let snapshot = store.snapshot(company_id).await?;
    if !snapshot.required
        || plan_reminder(store, company_id, &snapshot, now, true)
            .await?
            .is_none()
    {
        return Ok(());
    }
    while !limit_reached(effects, limits, result) {
        let lock = effects.lock(company_id).await?;
        let sent_one = send_next_locked(store, effects, company_id, now, result).await;
        effects.unlock(lock).await;
        if !sent_one? {
            break;
        }
    }
    Ok(())
}

/// One company's first-charge notices. No company lock: there is nothing to
/// settle, and the unique claim is the idempotency.
///
/// The stored `trial_end_at` picks the candidate; Stripe's `trial_end` dates
/// the email and keys its cycle. When Stripe says nothing is due, or its date
/// differs from the stored one, the stored cycle gets a `skipped` row so it
/// is not asked again before `FIRST_CHARGE_RECHECK_HOURS`.
async fn first_charge_company<S: ReminderStore, E: ReminderEffects>(
    store: &S,
    effects: &E,
    company_id: i32,
    now: NaiveDateTime,
    limits: ReminderLimits,
    result: &mut ReminderTickResult,
) -> Result<(), ReminderError> {
    let Some(record) = store.snapshot(company_id).await?.record else {
        return Ok(());
    };
    let Some(stored) = first_charge_due(&record, now) else {
        return Ok(());
    };
    let contacts = recipients(store, company_id).await?;
    let stored_rows = store.reminders(company_id, stored.cycle).await?;
    if open_contacts(contacts.clone(), &stored_rows, stored.step, now).is_empty() {
        return Ok(());
    }
    // Never claim without a quote: the email states the amount and date.
    let quote = effects.first_charge_quote(&record).await?;
    let Some((quote, due)) = quote.and_then(|quote| {
        let due = first_charge_notice(quote.charge_at, now)?;
        Some((quote, due))
    }) else {
        store
            .skip_cycle(company_id, stored.cycle, stored.step, now)
            .await?;
        return Ok(());
    };
    let stale = due.cycle != stored.cycle;
    let open = if stale {
        let rows = store.reminders(company_id, due.cycle).await?;
        open_contacts(contacts, &rows, due.step, now)
    } else {
        open_contacts(contacts, &stored_rows, due.step, now)
    };
    let company_name = store.company_name(company_id).await?;
    for contact in open {
        if limit_reached(effects, limits, result) {
            return Ok(());
        }
        let key = ReminderKey {
            company_id,
            user_id: contact.user_id,
            cycle: due.cycle,
            step: due.step,
        };
        let Some(id) = store.claim(&key, now).await? else {
            continue;
        };
        result.processed += 1;
        let claimed = ClaimedReminder { id, company_id };
        let rendered = render_first_charge_email(&FirstChargeEmailInput {
            company_name: &company_name,
            user_name: contact.name.as_deref(),
            quote: &quote,
            app_url: effects.app_url(),
        });
        let email = ReminderEmail {
            to: contact.email,
            subject: rendered.subject,
            html: rendered.html,
            text: rendered.text,
        };
        let context = send_context(claimed, contact.user_id, due.cycle, due.step);
        send_claimed(store, effects, claimed, &email, &context, now, result).await?;
    }
    // Every manager is handled under Stripe's date: the stale stored one can
    // rest until the webhook (or a recheck) brings the record in line.
    if stale {
        store
            .skip_cycle(company_id, stored.cycle, stored.step, now)
            .await?;
    }
    Ok(())
}

async fn report_company_error<E: ReminderEffects>(
    effects: &E,
    company_id: i32,
    kind: Option<DunningKind>,
    error: &ReminderError,
    result: &mut ReminderTickResult,
) {
    result.company_errors += 1;
    let context = ReportContext {
        company_id: Some(company_id),
        kind,
        error_code: Some(error_code_of(error).into()),
        ..ReportContext::default()
    };
    effects
        .report("Billing reminders failed for a company", &context)
        .await;
}

/// One tick: sweeps stuck claims, then dunning reminders (under the company
/// lock), then first-charge notices, within `limits`.
pub async fn process_billing_reminders<S: ReminderStore, E: ReminderEffects>(
    store: &S,
    effects: &E,
    now: NaiveDateTime,
    limits: ReminderLimits,
) -> ReminderTickResult {
    let mut result = ReminderTickResult::default();
    match store
        .sweep_stuck(now - Duration::minutes(STUCK_SENDING_MINUTES))
        .await
    {
        Ok(stuck) => {
            for row in &stuck {
                let context = ReportContext {
                    company_id: Some(row.key.company_id),
                    reminder_id: Some(row.id),
                    user_id: Some(row.key.user_id),
                    kind: Some(row.key.cycle.kind),
                    step: Some(row.key.step),
                    error_code: None,
                };
                effects
                    .report("Billing reminder stuck in sending, not resent", &context)
                    .await;
            }
            result.stuck = stuck.len();
        }
        Err(error) => {
            let context = ReportContext {
                error_code: Some(error_code_of(&ReminderError::from(error)).into()),
                ..ReportContext::default()
            };
            effects
                .report("Billing reminder sweep failed", &context)
                .await;
        }
    }

    match store.candidate_company_ids(now).await {
        Ok(ids) => {
            for company_id in ids {
                if limit_reached(effects, limits, &result) {
                    break;
                }
                if let Err(error) =
                    dunning_company(store, effects, company_id, now, limits, &mut result).await
                {
                    report_company_error(effects, company_id, None, &error, &mut result).await;
                }
            }
        }
        Err(error) => {
            report_list_failure(effects, &ReminderError::from(error)).await;
        }
    }

    match store.first_charge_company_ids(now).await {
        Ok(ids) => {
            for company_id in ids {
                if limit_reached(effects, limits, &result) {
                    break;
                }
                if let Err(error) =
                    first_charge_company(store, effects, company_id, now, limits, &mut result).await
                {
                    let kind = Some(DunningKind::FirstCharge);
                    report_company_error(effects, company_id, kind, &error, &mut result).await;
                }
            }
        }
        Err(error) => {
            report_list_failure(effects, &ReminderError::from(error)).await;
        }
    }
    result
}

async fn report_list_failure<E: ReminderEffects>(effects: &E, error: &ReminderError) {
    let context = ReportContext {
        error_code: Some(error_code_of(error).into()),
        ..ReportContext::default()
    };
    effects
        .report("Billing reminder company list failed", &context)
        .await;
}

#[cfg(test)]
mod tests {
    //! Port of the CRM's `billingReminders.test.ts` against in-memory fakes,
    //! plus the first-charge notice.
    use super::*;
    use crate::billing::gate::{GateStripe, settle_locked};
    use crate::billing::machine::apply_event;
    use crate::billing::test_store::{Memory, at};
    use crate::billing::{BillingEvent, InvoiceSnapshot, StripeLiveState, SubscriptionSnapshot};
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    const APP_URL: &str = "https://app.example.com";
    const FAILED_SUBJECT: &str = "Action needed: your Granite Manager payment did not go through";
    const THREE_DAYS_SUBJECT: &str = "3 days left to update your payment";
    const TOMORROW_SUBJECT: &str = "Access to Granite Manager will be paused tomorrow";
    const PAUSED_SUBJECT: &str = "Access to Granite Manager is paused";
    const RECOVERED_SUBJECT: &str = "Payment received, access restored";
    const SUBSCRIBE_SUBJECT: &str = "Subscribe to keep using Granite Manager";
    const SUBSCRIBE_THREE_DAYS_SUBJECT: &str = "3 days left to subscribe";
    const SUBSCRIBED_SUBJECT: &str = "Your Granite Manager subscription is active";

    fn failed_at() -> NaiveDateTime {
        at("2026-10-01 15:00:00")
    }

    /// `days` after the failed payment, plus `extra_secs`.
    fn t(days: i64, extra_secs: i64) -> NaiveDateTime {
        failed_at() + Duration::days(days) + Duration::seconds(extra_secs)
    }

    #[derive(Clone, Copy)]
    enum SendMode {
        Sent,
        Rejected,
        Unknown,
    }

    enum QuoteMode {
        /// Stripe agrees with the stored `trial_end_at`.
        Quote(FirstChargeQuote),
        /// Stripe's `trial_end` differs from the stored one.
        StripeSays(NaiveDateTime),
        Missing,
        Fails,
    }

    struct Stored {
        row: ReminderRow,
    }

    struct Fake {
        billing: Memory,
        contacts: RefCell<Vec<ReminderContact>>,
        rows: RefCell<Vec<Stored>>,
        sent: RefCell<Vec<ReminderEmail>>,
        reports: RefCell<Vec<(String, ReportContext)>>,
        live: RefCell<Option<StripeLiveState>>,
        refresh: Cell<fn(CompanyBilling) -> CompanyBilling>,
        mode: Cell<SendMode>,
        send_calls: Cell<usize>,
        quote: RefCell<QuoteMode>,
        quote_calls: Cell<usize>,
        held: Cell<bool>,
        busy: Cell<bool>,
        elapsed: Cell<std::time::Duration>,
        send_cost: Cell<std::time::Duration>,
        /// A payment that was queued on the lock during a send: it lands the
        /// moment the lock is released, before the next email.
        pay_on_unlock: Cell<Option<NaiveDateTime>>,
    }

    fn contact(user_id: i32, name: &str, email: &str) -> ReminderContact {
        ReminderContact {
            user_id,
            name: Some(name.into()),
            email: email.into(),
        }
    }

    fn sample_quote() -> FirstChargeQuote {
        FirstChargeQuote {
            charge_at: trial_ends(),
            amount_cents: 30_000,
            lines: vec![ChargeLine {
                label: "Base plan, up to 10 users".into(),
                amount_cents: 30_000,
            }],
            payment_method: Some("Visa ending in 4242".into()),
        }
    }

    fn fake() -> Fake {
        let billing = Memory::default();
        // Users 10 and 11 are managers, 12 is a sales rep.
        *billing.users.borrow_mut() = vec![(10, 1, true), (11, 1, true), (12, 1, false)];
        Fake {
            billing,
            contacts: RefCell::new(vec![
                contact(10, "Ann", "ann@example.com"),
                contact(11, "Bob", "bob@example.com"),
                contact(12, "Sam", "sam@example.com"),
            ]),
            rows: RefCell::default(),
            sent: RefCell::default(),
            reports: RefCell::default(),
            live: RefCell::default(),
            refresh: Cell::new(|record| record),
            mode: Cell::new(SendMode::Sent),
            send_calls: Cell::new(0),
            quote: RefCell::new(QuoteMode::Quote(sample_quote())),
            quote_calls: Cell::new(0),
            held: Cell::new(false),
            busy: Cell::new(false),
            elapsed: Cell::new(std::time::Duration::ZERO),
            send_cost: Cell::new(std::time::Duration::ZERO),
            pay_on_unlock: Cell::new(None),
        }
    }

    fn db_down() -> sqlx::Error {
        sqlx::Error::Protocol("down".into())
    }

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    impl ReminderStore for Fake {
        async fn candidate_company_ids(&self, _: NaiveDateTime) -> StoreResult<Vec<i32>> {
            Ok(vec![1])
        }
        async fn first_charge_company_ids(&self, now: NaiveDateTime) -> StoreResult<Vec<i32>> {
            Ok(self
                .billing
                .records
                .borrow()
                .values()
                .filter(|record| {
                    first_charge_due(record, now).is_some_and(|due| {
                        !self.rows.borrow().iter().any(|stored| {
                            stored.row.status == ReminderStatus::Skipped
                                && stored.row.key.cycle == due.cycle
                                && now - stored.row.claimed_at
                                    < Duration::hours(FIRST_CHARGE_RECHECK_HOURS)
                        })
                    })
                })
                .map(|record| record.company_id)
                .collect())
        }
        async fn snapshot(&self, company_id: i32) -> StoreResult<CompanyBillingSnapshot> {
            Ok(CompanyBillingSnapshot {
                required: !self.billing.not_required.borrow().contains(&company_id),
                required_since: self
                    .billing
                    .required_since
                    .borrow()
                    .get(&company_id)
                    .copied(),
                record: self.billing.records.borrow().get(&company_id).cloned(),
            })
        }
        async fn managers(&self, company_id: i32) -> StoreResult<Vec<i32>> {
            use crate::billing::store::BillingStore;
            self.billing.managers(company_id).await
        }
        async fn contacts(&self, _: i32, user_ids: &[i32]) -> StoreResult<Vec<ReminderContact>> {
            Ok(self
                .contacts
                .borrow()
                .iter()
                .filter(|contact| user_ids.contains(&contact.user_id))
                .cloned()
                .collect())
        }
        async fn company_name(&self, _: i32) -> StoreResult<String> {
            Ok("Granite Co".into())
        }
        async fn reminders(
            &self,
            company_id: i32,
            cycle: DunningCycle,
        ) -> StoreResult<Vec<ReminderRow>> {
            Ok(self
                .rows
                .borrow()
                .iter()
                .map(|stored| stored.row)
                .filter(|row| row.key.company_id == company_id && row.key.cycle == cycle)
                .collect())
        }
        async fn claim(&self, key: &ReminderKey, now: NaiveDateTime) -> StoreResult<Option<i64>> {
            let mut rows = self.rows.borrow_mut();
            if let Some(stored) = rows.iter_mut().find(|stored| stored.row.key == *key) {
                if !can_claim(Some(&stored.row), now) {
                    return Ok(None);
                }
                stored.row.status = ReminderStatus::Sending;
                stored.row.attempts += 1;
                stored.row.claimed_at = now;
                return Ok(Some(stored.row.id));
            }
            let id = i64::try_from(rows.len()).unwrap_or(0) + 1;
            rows.push(Stored {
                row: ReminderRow {
                    id,
                    key: *key,
                    status: ReminderStatus::Sending,
                    attempts: 1,
                    claimed_at: now,
                },
            });
            Ok(Some(id))
        }
        async fn skip_cycle(
            &self,
            company_id: i32,
            cycle: DunningCycle,
            step: ReminderStep,
            now: NaiveDateTime,
        ) -> StoreResult<()> {
            let key = ReminderKey {
                company_id,
                user_id: NO_USER,
                cycle,
                step,
            };
            let mut rows = self.rows.borrow_mut();
            if let Some(stored) = rows.iter_mut().find(|stored| stored.row.key == key) {
                stored.row.claimed_at = now;
                return Ok(());
            }
            let id = i64::try_from(rows.len()).unwrap_or(0) + 1;
            rows.push(Stored {
                row: ReminderRow {
                    id,
                    key,
                    status: ReminderStatus::Skipped,
                    attempts: 0,
                    claimed_at: now,
                },
            });
            Ok(())
        }
        async fn mark_sent(
            &self,
            row: ClaimedReminder,
            _: Option<&str>,
            _: NaiveDateTime,
        ) -> StoreResult<()> {
            self.set_status(row.id, ReminderStatus::Sent);
            Ok(())
        }
        async fn mark_failed(&self, row: ClaimedReminder, _: &str) -> StoreResult<()> {
            self.set_status(row.id, ReminderStatus::Failed);
            Ok(())
        }
        async fn mark_unknown(&self, row: ClaimedReminder, _: &str) -> StoreResult<()> {
            self.set_status(row.id, ReminderStatus::Unknown);
            Ok(())
        }
        async fn sweep_stuck(&self, cutoff: NaiveDateTime) -> StoreResult<Vec<ReminderRow>> {
            let mut swept = Vec::new();
            for stored in self.rows.borrow_mut().iter_mut() {
                if stored.row.status == ReminderStatus::Sending && stored.row.claimed_at < cutoff {
                    stored.row.status = ReminderStatus::Unknown;
                    swept.push(stored.row);
                }
            }
            Ok(swept)
        }
    }

    impl Fake {
        fn set_status(&self, id: i64, status: ReminderStatus) {
            for stored in self.rows.borrow_mut().iter_mut() {
                if stored.row.id == id {
                    stored.row.status = status;
                }
            }
        }
    }

    struct Gate<'a>(&'a Fake);

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    impl GateStripe for Gate<'_> {
        type Lock = ();
        fn check_due(&self, _: &CompanyBilling) -> bool {
            true
        }
        async fn refresh(&self, record: CompanyBilling) -> CompanyBilling {
            let refreshed = (self.0.refresh.get())(record);
            self.0
                .billing
                .records
                .borrow_mut()
                .insert(refreshed.company_id, refreshed.clone());
            refreshed
        }
        async fn live_state(&self, _: &CompanyBilling) -> Option<StripeLiveState> {
            self.0.live.borrow().clone()
        }
        async fn lock(&self, _: i32) -> Option<()> {
            Some(())
        }
        async fn unlock(&self, (): ()) {}
    }

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    impl ReminderEffects for Fake {
        type Lock = ();
        async fn lock(&self, _: i32) -> Result<(), ReminderError> {
            if self.busy.get() || self.held.get() {
                return Err(ReminderError::Busy);
            }
            self.held.set(true);
            Ok(())
        }
        async fn unlock(&self, (): ()) {
            self.held.set(false);
            if let Some(now) = self.pay_on_unlock.take() {
                apply_event(&self.billing, &invoice_event("evt_paid_queued", true), now)
                    .await
                    .ok();
            }
        }
        async fn settle(
            &self,
            company_id: i32,
            required_since: Option<NaiveDateTime>,
            now: NaiveDateTime,
        ) -> Result<Option<CompanyBilling>, ReminderError> {
            Ok(settle_locked(&self.billing, &Gate(self), company_id, required_since, now).await?)
        }
        async fn first_charge_quote(
            &self,
            record: &CompanyBilling,
        ) -> Result<Option<FirstChargeQuote>, ReminderError> {
            self.quote_calls.set(self.quote_calls.get() + 1);
            match &*self.quote.borrow() {
                QuoteMode::Quote(quote) => Ok(Some(FirstChargeQuote {
                    charge_at: record.trial_end_at.unwrap_or(quote.charge_at),
                    ..quote.clone()
                })),
                QuoteMode::StripeSays(charge_at) => Ok(Some(FirstChargeQuote {
                    charge_at: *charge_at,
                    ..sample_quote()
                })),
                QuoteMode::Missing => Ok(None),
                QuoteMode::Fails => Err(ReminderError::Stripe("down".into())),
            }
        }
        async fn send(&self, email: &ReminderEmail) -> ReminderSendResult {
            self.send_calls.set(self.send_calls.get() + 1);
            // Yield like a real send, so concurrent runs interleave here.
            tokio::task::yield_now().await;
            self.elapsed.set(self.elapsed.get() + self.send_cost.get());
            match self.mode.get() {
                SendMode::Sent => {
                    self.sent.borrow_mut().push(email.clone());
                    ReminderSendResult::Sent {
                        message_id: Some("msg".into()),
                    }
                }
                SendMode::Rejected => ReminderSendResult::Rejected {
                    code: "MessageRejected".into(),
                },
                SendMode::Unknown => {
                    self.sent.borrow_mut().push(email.clone());
                    ReminderSendResult::Unknown {
                        code: "SendTimeout".into(),
                    }
                }
            }
        }
        async fn report(&self, problem: &str, context: &ReportContext) {
            self.reports
                .borrow_mut()
                .push((problem.to_string(), context.clone()));
        }
        fn elapsed(&self) -> std::time::Duration {
            self.elapsed.get()
        }
        fn app_url(&self) -> &str {
            APP_URL
        }
    }

    fn invoice_event(id: &str, paid: bool) -> BillingEvent {
        BillingEvent::Invoice {
            id: id.into(),
            paid,
            invoice: InvoiceSnapshot {
                id: format!("in_{id}"),
                customer_id: "cus_1".into(),
                subscription_id: Some("sub_1".into()),
                company_id: Some(1),
                status: if paid { "paid" } else { "open" }.into(),
                amount_due_cents: 30_000,
                amount_paid_cents: if paid { 30_000 } else { 0 },
                currency: "usd".into(),
                hosted_invoice_url: None,
                invoice_pdf: None,
                paid_at: None,
                live_subscription_status: paid.then(|| "active".to_string()),
                replaced_by: None,
            },
        }
    }

    fn subscription_active_event(id: &str) -> BillingEvent {
        BillingEvent::Subscription {
            id: id.into(),
            kind: "customer.subscription.updated",
            subscription: SubscriptionSnapshot {
                id: "sub_1".into(),
                customer_id: "cus_1".into(),
                price_id: Some("price_monthly".into()),
                item_id: Some("si_1".into()),
                status: "active".into(),
                quantity: Some(1),
                period_start: None,
                period_end: None,
                payment_method_id: Some("pm_1".into()),
                plan_name: Some("Granite Manager".into()),
                unit_amount_cents: Some(30_000),
                company_id: Some(1),
                deleted: false,
                items: Vec::new(),
                monthly_total_cents: None,
                trial_end: None,
                cancel_at: None,
                replaced_by: None,
            },
        }
    }

    impl Fake {
        async fn fail(&self) {
            apply_event(
                &self.billing,
                &invoice_event("evt_failed", false),
                failed_at(),
            )
            .await
            .unwrap();
        }
        async fn pay(&self, now: NaiveDateTime) {
            let event = invoice_event(&format!("evt_paid_{now}"), true);
            apply_event(&self.billing, &event, now).await.unwrap();
        }
        async fn tick(&self, now: NaiveDateTime) -> ReminderTickResult {
            process_billing_reminders(self, self, now, ReminderLimits::default()).await
        }
        async fn tick_max(&self, now: NaiveDateTime, max_emails: usize) -> ReminderTickResult {
            let limits = ReminderLimits {
                max_emails,
                ..ReminderLimits::default()
            };
            process_billing_reminders(self, self, now, limits).await
        }
        fn subjects(&self) -> Vec<String> {
            self.sent
                .borrow()
                .iter()
                .map(|email| format!("{}: {}", email.to, email.subject))
                .collect()
        }
        fn sent_count(&self) -> usize {
            self.sent.borrow().len()
        }
        fn status(&self, company_id: i32) -> PaymentStatus {
            self.billing.records.borrow()[&company_id].payment_status
        }
        fn statuses(&self) -> Vec<(i32, ReminderStatus, i32)> {
            self.rows
                .borrow()
                .iter()
                .map(|stored| {
                    (
                        stored.row.key.user_id,
                        stored.row.status,
                        stored.row.attempts,
                    )
                })
                .collect()
        }
    }

    fn pair(subject: &str) -> Vec<String> {
        vec![
            format!("ann@example.com: {subject}"),
            format!("bob@example.com: {subject}"),
        ]
    }

    fn tail(all: &[String], skip: usize) -> Vec<String> {
        all[skip..].to_vec()
    }

    /// A company asked to pay at `failed_at()` that has not subscribed.
    fn first_payment() -> Fake {
        let fake = fake();
        fake.billing
            .required_since
            .borrow_mut()
            .insert(1, failed_at());
        fake
    }

    fn paid_in_stripe(record: CompanyBilling) -> CompanyBilling {
        CompanyBilling {
            stripe_subscription_id: Some("sub_1".into()),
            subscription_status: "active".into(),
            payment_status: PaymentStatus::Active,
            ..record
        }
    }

    fn with_customer() -> CompanyBilling {
        CompanyBilling {
            stripe_customer_id: Some("cus_1".into()),
            ..CompanyBilling::empty(1)
        }
    }

    // ---- billing reminder schedule

    #[tokio::test]
    async fn day_0_day_4_and_day_6_go_to_managers_only_on_utc_boundaries() {
        let fake = fake();
        fake.fail().await;

        fake.tick(t(0, 60)).await;
        assert_eq!(fake.subjects(), pair(FAILED_SUBJECT));

        fake.tick(t(4, -1)).await;
        assert_eq!(fake.sent_count(), 2);
        fake.tick(t(4, 0)).await;
        assert_eq!(tail(&fake.subjects(), 2), pair(THREE_DAYS_SUBJECT));

        fake.tick(t(6, -1)).await;
        assert_eq!(fake.sent_count(), 4);
        fake.tick(t(6, 0)).await;
        fake.tick(t(6, 60)).await;
        assert_eq!(tail(&fake.subjects(), 4), pair(TOMORROW_SUBJECT));
        assert!(
            fake.sent.borrow()[4]
                .text
                .contains("paused tomorrow, October 8, 2026")
        );
    }

    #[tokio::test]
    async fn catch_up_after_missed_ticks_sends_only_the_latest_due_step() {
        let fake = fake();
        fake.fail().await;

        fake.tick(t(6, 12 * 3600)).await;

        assert_eq!(fake.subjects(), pair(TOMORROW_SUBJECT));
    }

    #[tokio::test]
    async fn a_cycle_already_past_day_7_gets_only_the_paused_email() {
        let fake = fake();
        *fake.live.borrow_mut() = Some(StripeLiveState {
            subscription_status: Some("past_due".into()),
            open_invoice_cents: 30_000,
        });
        fake.fail().await;

        fake.tick(t(7, 60)).await;

        assert_eq!(fake.status(1), PaymentStatus::Suspended);
        assert_eq!(fake.subjects(), pair(PAUSED_SUBJECT));
        assert!(
            fake.sent.borrow()[0]
                .text
                .contains("was paused on October 8, 2026")
        );
    }

    #[tokio::test]
    async fn no_paused_email_and_no_pause_until_stripe_confirms_the_non_payment() {
        let fake = fake();
        fake.fail().await;
        fake.tick(t(6, 0)).await;
        assert_eq!(fake.sent_count(), 2);

        *fake.live.borrow_mut() = None;
        fake.tick(t(7, 60)).await;
        fake.tick(t(7, 120)).await;
        assert_eq!(fake.status(1), PaymentStatus::PastDue);
        assert_eq!(fake.sent_count(), 2);

        *fake.live.borrow_mut() = Some(StripeLiveState {
            subscription_status: Some("past_due".into()),
            open_invoice_cents: 30_000,
        });
        fake.tick(t(7, 180)).await;
        assert_eq!(fake.status(1), PaymentStatus::Suspended);
        assert_eq!(tail(&fake.subjects(), 2), pair(PAUSED_SUBJECT));
    }

    #[tokio::test]
    async fn grace_over_and_stripe_shows_the_company_paid_restored_recovery_email_instead() {
        let fake = fake();
        fake.fail().await;
        fake.tick(t(0, 60)).await;

        *fake.live.borrow_mut() = Some(StripeLiveState {
            subscription_status: Some("active".into()),
            open_invoice_cents: 0,
        });
        fake.tick(t(7, 60)).await;

        assert_eq!(fake.status(1), PaymentStatus::Active);
        assert_eq!(tail(&fake.subjects(), 2), pair(RECOVERED_SUBJECT));
    }

    #[tokio::test]
    async fn billing_switched_off_no_emails() {
        let fake = fake();
        fake.fail().await;
        fake.billing.not_required.borrow_mut().insert(1);

        fake.tick(t(0, 60)).await;
        fake.tick(t(4, 0)).await;

        assert_eq!(fake.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_cycle_older_than_14_days_is_left_alone() {
        let fake = fake();
        fake.fail().await;
        fake.billing
            .records
            .borrow_mut()
            .get_mut(&1)
            .unwrap()
            .payment_status = PaymentStatus::Suspended;

        fake.tick(t(14, 0)).await;

        assert_eq!(fake.sent_count(), 0);
    }

    // ---- billing reminders never duplicate

    #[tokio::test]
    async fn overlapping_and_repeated_ticks_send_each_step_once_per_manager() {
        let fake = fake();
        fake.fail().await;

        tokio::join!(
            fake.tick(t(0, 60)),
            fake.tick(t(0, 60)),
            fake.tick(t(0, 61))
        );
        fake.tick(t(0, 120)).await;

        assert_eq!(fake.subjects(), pair(FAILED_SUBJECT));
    }

    #[tokio::test]
    async fn a_rejected_send_is_retried_at_most_three_times_in_total() {
        let fake = fake();
        fake.mode.set(SendMode::Rejected);
        fake.fail().await;

        // A refused send waits 5 minutes before its next try.
        for minute in 1..=5 {
            fake.tick(t(0, minute * 60)).await;
        }
        assert_eq!(fake.send_calls.get(), 2);
        for minute in 6..=20 {
            fake.tick(t(0, minute * 60)).await;
        }

        assert_eq!(fake.send_calls.get(), 6);
        assert_eq!(
            fake.statuses(),
            vec![
                (10, ReminderStatus::Failed, 3),
                (11, ReminderStatus::Failed, 3)
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_outcome_is_never_resent_and_is_reported() {
        let fake = fake();
        fake.mode.set(SendMode::Unknown);
        fake.fail().await;

        fake.tick(t(0, 60)).await;
        fake.tick(t(0, 120)).await;

        assert_eq!(fake.sent_count(), 2);
        assert_eq!(
            fake.statuses()
                .iter()
                .map(|status| status.1)
                .collect::<Vec<_>>(),
            vec![ReminderStatus::Unknown; 2]
        );
        assert_eq!(fake.reports.borrow().len(), 2);
        assert!(!format!("{:?}", fake.reports.borrow()).contains('@'));
    }

    #[tokio::test]
    async fn a_crash_between_send_and_record_is_reported_and_not_resent() {
        let fake = fake();
        fake.fail().await;
        let key = ReminderKey {
            company_id: 1,
            user_id: 10,
            cycle: DunningCycle {
                kind: DunningKind::PaymentFailed,
                started_at: failed_at(),
            },
            step: ReminderStep::Started,
        };
        fake.claim(&key, t(0, 60)).await.unwrap();

        let result = fake
            .tick(t(0, 60) + Duration::minutes(STUCK_SENDING_MINUTES) + Duration::seconds(1))
            .await;

        assert_eq!(result.stuck, 1);
        assert_eq!(fake.statuses()[0], (10, ReminderStatus::Unknown, 1));
        assert!(
            fake.reports
                .borrow()
                .iter()
                .any(|(_, context)| context.user_id == Some(10))
        );
        assert_eq!(
            fake.subjects(),
            vec![format!("bob@example.com: {FAILED_SUBJECT}")]
        );
    }

    #[tokio::test]
    async fn each_tick_sends_at_most_its_budget_the_rest_follows_next_tick() {
        let fake = fake();
        fake.fail().await;

        let first = fake.tick_max(t(0, 60), 1).await;
        assert_eq!(first.sent, 1);
        fake.tick_max(t(0, 120), 1).await;

        let to: Vec<String> = fake.sent.borrow().iter().map(|e| e.to.clone()).collect();
        assert_eq!(to, ["ann@example.com", "bob@example.com"]);
    }

    // ---- billing reminders stay in step with payments

    #[tokio::test]
    async fn a_payment_landing_between_two_emails_stops_the_rest_of_the_countdown() {
        let fake = fake();
        fake.fail().await;
        fake.pay_on_unlock.set(Some(t(0, 90)));

        fake.tick(t(0, 120)).await;

        assert_eq!(fake.status(1), PaymentStatus::Active);
        assert_eq!(
            fake.subjects(),
            vec![
                format!("ann@example.com: {FAILED_SUBJECT}"),
                format!("ann@example.com: {RECOVERED_SUBJECT}"),
                format!("bob@example.com: {RECOVERED_SUBJECT}"),
            ]
        );
    }

    #[tokio::test]
    async fn two_manager_accounts_with_one_mailbox_get_one_email() {
        let fake = fake();
        for contact in fake.contacts.borrow_mut().iter_mut() {
            if contact.user_id == 11 {
                contact.email = " ANN@example.com".into();
            }
        }
        fake.fail().await;

        fake.tick(t(0, 60)).await;
        fake.tick(t(4, 0)).await;

        assert_eq!(
            fake.subjects(),
            vec![
                format!("ann@example.com: {FAILED_SUBJECT}"),
                format!("ann@example.com: {THREE_DAYS_SUBJECT}"),
            ]
        );
    }

    // ---- payment received email

    #[tokio::test]
    async fn paying_stops_the_countdown_and_sends_the_recovery_email_exactly_once() {
        let fake = fake();
        fake.fail().await;
        fake.tick(t(0, 60)).await;

        fake.pay(t(3, 0)).await;
        tokio::join!(fake.tick(t(4, 0)), fake.tick(t(4, 0)));
        fake.tick(t(6, 0)).await;

        assert_eq!(tail(&fake.subjects(), 2), pair(RECOVERED_SUBJECT));
    }

    #[tokio::test]
    async fn no_recovery_email_for_a_cycle_nobody_was_emailed_about() {
        let fake = fake();
        fake.fail().await;
        fake.pay(t(0, 30)).await;

        fake.tick(t(0, 60)).await;

        assert_eq!(fake.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_new_failure_replaces_an_unsent_recovery() {
        let fake = fake();
        fake.fail().await;
        fake.tick(t(0, 60)).await;
        fake.pay(t(1, 0)).await;
        apply_event(
            &fake.billing,
            &invoice_event("evt_failed_again", false),
            t(1, 1),
        )
        .await
        .unwrap();

        fake.tick(t(1, 60)).await;

        assert_eq!(tail(&fake.subjects(), 2), pair(FAILED_SUBJECT));
    }

    // ---- a company asked to pay that never subscribed

    #[tokio::test]
    async fn first_payment_day_0_4_and_6_count_from_payment_turned_on_then_the_pause() {
        let fake = first_payment();

        fake.tick(t(0, 60)).await;
        fake.tick(t(4, 0)).await;
        fake.tick(t(6, 0)).await;
        fake.tick(t(7, 60)).await;
        fake.tick(t(7, 120)).await;

        let expected: Vec<String> = [
            SUBSCRIBE_SUBJECT,
            SUBSCRIBE_THREE_DAYS_SUBJECT,
            TOMORROW_SUBJECT,
            PAUSED_SUBJECT,
        ]
        .iter()
        .flat_map(|subject| pair(subject))
        .collect();
        assert_eq!(fake.subjects(), expected);
        let sent = fake.sent.borrow();
        assert!(sent[0].text.contains("Please subscribe by October 8, 2026"));
        assert!(
            sent[6]
                .text
                .contains("has not subscribed, so access for your team")
        );
    }

    #[tokio::test]
    async fn stripe_shows_a_payment_at_the_deadline_no_pause_the_subscribed_email_instead() {
        let fake = first_payment();
        fake.refresh.set(paid_in_stripe);
        fake.billing.records.borrow_mut().insert(1, with_customer());
        fake.tick(t(0, 60)).await;

        fake.tick(t(7, 60)).await;

        assert_eq!(fake.status(1), PaymentStatus::Active);
        assert_eq!(tail(&fake.subjects(), 2), pair(SUBSCRIBED_SUBJECT));
    }

    #[tokio::test]
    async fn subscribing_stops_the_countdown_and_sends_the_subscribed_email_once() {
        let fake = first_payment();
        fake.tick(t(0, 60)).await;

        apply_event(
            &fake.billing,
            &subscription_active_event("evt_sub"),
            t(2, 0),
        )
        .await
        .unwrap();
        tokio::join!(fake.tick(t(4, 0)), fake.tick(t(4, 0)));
        fake.tick(t(6, 0)).await;
        fake.tick(t(7, 60)).await;

        assert_eq!(tail(&fake.subjects(), 2), pair(SUBSCRIBED_SUBJECT));
    }

    #[tokio::test]
    async fn subscribed_before_anyone_was_emailed_no_subscribed_email() {
        let fake = first_payment();
        apply_event(
            &fake.billing,
            &subscription_active_event("evt_sub"),
            t(0, 30),
        )
        .await
        .unwrap();

        fake.tick(t(0, 60)).await;

        assert_eq!(fake.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_company_that_already_pays_gets_no_first_payment_emails() {
        let fake = first_payment();
        fake.billing
            .records
            .borrow_mut()
            .insert(1, paid_in_stripe(with_customer()));

        fake.tick(t(0, 60)).await;
        fake.tick(t(7, 60)).await;

        assert_eq!(fake.sent_count(), 0);
    }

    #[tokio::test]
    async fn no_billing_required_at_or_payment_switched_off_no_emails() {
        let unknown_start = fake();
        unknown_start.tick(t(0, 60)).await;
        assert_eq!(unknown_start.sent_count(), 0);

        let switched_off = first_payment();
        switched_off.billing.not_required.borrow_mut().insert(1);
        switched_off.tick(t(0, 60)).await;
        assert_eq!(switched_off.sent_count(), 0);
    }

    // ---- send failures and limits

    #[test]
    fn only_an_answered_refusal_is_safe_to_retry() {
        let rejected = classify_send_failure(&SendFailure::Rejected("bad ann@example.com".into()));
        let failed = classify_send_failure(&SendFailure::Failed("502 ann@example.com".into()));
        assert_eq!(
            rejected,
            ReminderSendResult::Rejected {
                code: "rejected".into()
            }
        );
        assert_eq!(
            failed,
            ReminderSendResult::Unknown {
                code: "send_failed".into()
            }
        );
    }

    #[tokio::test]
    async fn no_new_email_starts_once_the_deadline_has_passed() {
        let fake = fake();
        fake.fail().await;
        fake.send_cost.set(std::time::Duration::from_secs(50));

        let result = fake.tick(t(0, 60)).await;

        assert_eq!(result.sent, 1);
        assert_eq!(fake.sent_count(), 1);
    }

    #[tokio::test]
    async fn a_busy_lock_skips_the_company_and_the_next_tick_retries() {
        let fake = fake();
        fake.fail().await;
        fake.busy.set(true);

        let result = fake.tick(t(0, 60)).await;
        assert_eq!((result.company_errors, result.sent), (1, 0));
        assert_eq!(
            fake.reports.borrow()[0].1.error_code.as_deref(),
            Some("busy")
        );

        fake.busy.set(false);
        fake.tick(t(0, 120)).await;
        assert_eq!(fake.sent_count(), 2);
        assert!(!fake.held.get());
    }

    // ---- first charge

    fn trialing(trial_end_at: Option<NaiveDateTime>) -> CompanyBilling {
        CompanyBilling {
            stripe_customer_id: Some("cus_1".into()),
            stripe_subscription_id: Some("sub_1".into()),
            subscription_status: "trialing".into(),
            trial_end_at,
            ..CompanyBilling::empty(1)
        }
    }

    fn trial_ends() -> NaiveDateTime {
        at("2026-10-20 12:00:00")
    }

    fn with_trial(fake: &Fake) {
        fake.billing
            .records
            .borrow_mut()
            .insert(1, trialing(Some(trial_ends())));
    }

    fn first_charge_subjects(dates: &[&str]) -> Vec<String> {
        dates
            .iter()
            .flat_map(|date| pair(&format!("Your first Granite Manager charge is on {date}")))
            .collect()
    }

    #[test]
    fn first_charge_is_due_seven_days_before_and_not_otherwise() {
        let record = trialing(Some(trial_ends()));
        let due = first_charge_due(&record, trial_ends() - Duration::days(7)).unwrap();
        assert_eq!(
            due,
            DueReminder {
                cycle: DunningCycle {
                    kind: DunningKind::FirstCharge,
                    started_at: trial_ends()
                },
                step: ReminderStep::Upcoming,
                pauses_at: Some(trial_ends()),
            }
        );
        assert!(first_charge_due(&record, trial_ends() - Duration::days(2)).is_some());
        assert!(
            first_charge_due(
                &record,
                trial_ends() - Duration::days(7) - Duration::seconds(1)
            )
            .is_none()
        );
        assert!(first_charge_due(&record, trial_ends()).is_none());
        assert!(first_charge_due(&record, trial_ends() + Duration::hours(1)).is_none());
        let active = CompanyBilling {
            subscription_status: "active".into(),
            ..record
        };
        assert!(first_charge_due(&active, trial_ends() - Duration::days(3)).is_none());
        assert!(first_charge_due(&trialing(None), trial_ends() - Duration::days(3)).is_none());
        let cancelled = CompanyBilling {
            cancel_at: Some(trial_ends()),
            ..trialing(Some(trial_ends()))
        };
        assert!(first_charge_due(&cancelled, trial_ends() - Duration::days(3)).is_none());
        let cancelled_later = CompanyBilling {
            cancel_at: Some(trial_ends() + Duration::days(30)),
            ..trialing(Some(trial_ends()))
        };
        assert!(first_charge_due(&cancelled_later, trial_ends() - Duration::days(3)).is_some());
    }

    #[tokio::test]
    async fn first_charge_goes_once_to_each_manager_across_ticks() {
        let fake = fake();
        with_trial(&fake);

        let first = fake.tick(trial_ends() - Duration::days(7)).await;
        fake.tick(trial_ends() - Duration::days(6)).await;

        assert_eq!(first.sent, 2);
        assert_eq!(
            fake.subjects(),
            first_charge_subjects(&["October 20, 2026"])
        );
        assert_eq!(fake.quote_calls.get(), 1);
        let sent = fake.sent.borrow();
        assert!(sent[0].text.contains("$300.00 plus applicable sales tax"));
        assert!(sent[0].text.contains("Visa ending in 4242"));
    }

    #[tokio::test]
    async fn first_charge_ignores_the_billing_required_flag() {
        let fake = fake();
        with_trial(&fake);
        fake.billing.not_required.borrow_mut().insert(1);

        fake.tick(trial_ends() - Duration::days(5)).await;

        assert_eq!(fake.sent_count(), 2);
    }

    #[tokio::test]
    async fn a_cycle_first_seen_two_days_before_still_sends() {
        let fake = fake();
        with_trial(&fake);

        fake.tick(trial_ends() - Duration::days(2)).await;
        fake.tick(trial_ends() - Duration::days(1)).await;

        assert_eq!(
            fake.subjects(),
            first_charge_subjects(&["October 20, 2026"])
        );
    }

    #[tokio::test]
    async fn no_charge_is_skipped_and_stripe_is_asked_again_after_six_hours() {
        let fake = fake();
        with_trial(&fake);
        *fake.quote.borrow_mut() = QuoteMode::Missing;
        let start = trial_ends() - Duration::days(7);

        let missing = fake.tick(start).await;
        fake.tick(start + Duration::minutes(1)).await;
        fake.tick(start + Duration::hours(5)).await;

        assert_eq!(missing, ReminderTickResult::default());
        assert_eq!(fake.quote_calls.get(), 1);
        assert_eq!(fake.statuses(), vec![(NO_USER, ReminderStatus::Skipped, 0)]);
        fake.tick(start + Duration::hours(6) + Duration::minutes(1))
            .await;
        assert_eq!(fake.quote_calls.get(), 2);
        assert_eq!(fake.sent_count(), 0);
    }

    #[tokio::test]
    async fn a_failing_quote_claims_nothing() {
        let fake = fake();
        with_trial(&fake);
        *fake.quote.borrow_mut() = QuoteMode::Fails;

        let failing = fake.tick(trial_ends() - Duration::days(7)).await;

        assert_eq!(failing.company_errors, 1);
        assert_eq!(failing.processed, 0);
        assert!(fake.rows.borrow().is_empty());
    }

    #[tokio::test]
    async fn stripes_trial_end_dates_and_keys_the_email() {
        let fake = fake();
        with_trial(&fake);
        let stripe_date = trial_ends() + Duration::days(2);
        *fake.quote.borrow_mut() = QuoteMode::StripeSays(stripe_date);
        let now = trial_ends() - Duration::days(5);

        fake.tick(now).await;
        fake.tick(now + Duration::minutes(1)).await;

        assert_eq!(
            fake.subjects(),
            first_charge_subjects(&["October 22, 2026"])
        );
        assert!(fake.rows.borrow().iter().all(|stored| {
            stored.row.key.cycle.started_at
                == if stored.row.status == ReminderStatus::Skipped {
                    trial_ends()
                } else {
                    stripe_date
                }
        }));
        // The stale stored date rests: Stripe is not asked every minute.
        assert_eq!(fake.quote_calls.get(), 1);
    }

    #[tokio::test]
    async fn stripes_trial_end_outside_the_window_sends_nothing() {
        let fake = fake();
        with_trial(&fake);
        *fake.quote.borrow_mut() = QuoteMode::StripeSays(trial_ends() + Duration::days(30));

        fake.tick(trial_ends() - Duration::days(5)).await;

        assert_eq!(fake.sent_count(), 0);
        assert_eq!(fake.statuses(), vec![(NO_USER, ReminderStatus::Skipped, 0)]);
    }

    #[tokio::test]
    async fn a_changed_trial_end_is_a_new_cycle_and_sends_again() {
        let fake = fake();
        with_trial(&fake);
        fake.tick(trial_ends() - Duration::days(7)).await;

        let later = trial_ends() + Duration::days(3);
        fake.billing
            .records
            .borrow_mut()
            .insert(1, trialing(Some(later)));
        fake.tick(trial_ends() - Duration::days(4)).await;

        assert_eq!(
            fake.subjects(),
            first_charge_subjects(&["October 20, 2026", "October 23, 2026"])
        );
    }

    #[tokio::test]
    async fn first_charge_respects_the_email_budget() {
        let fake = fake();
        with_trial(&fake);

        fake.tick_max(trial_ends() - Duration::days(7), 1).await;
        assert_eq!(fake.sent_count(), 1);
        fake.tick_max(trial_ends() - Duration::days(6), 1).await;
        assert_eq!(fake.sent_count(), 2);
    }

    #[test]
    fn the_quote_comes_from_a_trialing_subscription() {
        let subscription = json!({
            "status": "trialing",
            "trial_end": 1_800_000_000,
            "items": {"data": [
                {"quantity": 1, "price": {"unit_amount": 30000, "metadata": {"kind": "base"}}},
                {"quantity": 4, "price": {"unit_amount": 3000, "metadata": {"kind": "seat"}}},
                {"quantity": 1, "price": {"unit_amount": 3000, "metadata": {"kind": "seat"}}},
                {"quantity": 1, "price": {"unit_amount": 2000, "metadata": {"addon_key": "quickbooks"}}},
                {"quantity": 1, "price": {"unit_amount": 1000, "metadata": {"addon_key": "signatures"}}},
                {"quantity": 1, "price": {"unit_amount": 500, "metadata": {"addon_key": "quotes"}}},
                {"quantity": 1, "price": {"unit_amount": 700, "metadata": {}}},
                {"quantity": 5, "price": {"unit_amount": 0, "metadata": {"kind": "seat"}}},
                {"quantity": 1, "price": {"metadata": {}}}
            ]}
        });

        let quote =
            first_charge_quote_from(&subscription, Some("Visa ending in 4242".into())).unwrap();

        let labels: Vec<&str> = quote.lines.iter().map(|line| line.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Base plan, up to 10 users",
                "4 active users above 10",
                "1 active user above 10",
                "QuickBooks add-on",
                "Signatures add-on",
                "Quotes add-on",
                "Subscription",
            ]
        );
        assert_eq!(quote.lines[1].amount_cents, 12_000);
        assert_eq!(
            quote.amount_cents,
            30_000 + 12_000 + 3_000 + 2_000 + 1_000 + 500 + 700
        );
        assert_eq!(quote.payment_method.as_deref(), Some("Visa ending in 4242"));
        assert_eq!(quote.charge_at, at("2027-01-15 08:00:00"));
        let mut undated = subscription;
        undated["trial_end"] = Value::Null;
        assert_eq!(first_charge_quote_from(&undated, None), None);
    }

    #[test]
    fn no_quote_without_a_trial_or_a_priced_line() {
        let item = json!({"quantity": 1, "price": {"unit_amount": 30000, "metadata": {}}});
        let active = json!({"status": "active", "items": {"data": [item]}});
        let free = json!({"status": "trialing", "items": {"data": [
            {"quantity": 1, "price": {"unit_amount": 0, "metadata": {}}}
        ]}});
        assert_eq!(first_charge_quote_from(&active, None), None);
        assert_eq!(first_charge_quote_from(&free, None), None);
        assert_eq!(
            first_charge_quote_from(&json!({"status": "trialing"}), None),
            None
        );
    }

    #[test]
    fn no_quote_when_the_trial_ends_in_a_cancellation() {
        let items = json!({"data": [
            {"quantity": 1, "price": {"unit_amount": 30000, "metadata": {"kind": "base"}}}
        ]});
        let trialing = |extra: Value| {
            let mut subscription =
                json!({"status": "trialing", "trial_end": 1_800_000_000, "items": items});
            if let (Some(target), Some(extra)) = (subscription.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            subscription
        };
        assert_eq!(
            first_charge_quote_from(&trialing(json!({"cancel_at_period_end": true})), None),
            None
        );
        assert_eq!(
            first_charge_quote_from(&trialing(json!({"cancel_at": 1_800_000_000})), None),
            None
        );
        assert!(
            first_charge_quote_from(&trialing(json!({"cancel_at": 1_900_000_000})), None).is_some()
        );
        assert!(
            first_charge_quote_from(&trialing(json!({"cancel_at_period_end": false})), None)
                .is_some()
        );
    }

    #[test]
    fn cards_are_named_from_stripes_brand_ids() {
        let card = |brand: &str| Card {
            id: "pm_1".into(),
            brand: brand.into(),
            last4: "4242".into(),
            exp_month: 1,
            exp_year: 2030,
        };
        assert_eq!(describe_card(&card("visa")), "Visa ending in 4242");
        assert_eq!(
            describe_card(&card("mastercard")),
            "Mastercard ending in 4242"
        );
        assert_eq!(
            describe_card(&card("amex")),
            "American Express ending in 4242"
        );
        assert_eq!(describe_card(&card("discover")), "Discover ending in 4242");
        assert_eq!(describe_card(&card("unionpay")), "Unionpay ending in 4242");
        assert_eq!(describe_card(&card("")), "Card ending in 4242");
    }

    #[test]
    fn db_errors_are_reported_by_code_only() {
        assert_eq!(
            error_code_of(&ReminderError::Database(db_down())),
            "database"
        );
    }
}
