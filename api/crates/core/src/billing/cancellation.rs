//! Cancellation requests: a Manager's "Request cancellation" on the payments
//! app's account page, and "Keep my subscription" to withdraw it (Terms 9).
//!
//! A request sets the Stripe subscription to end at its period end (the
//! trial end while trialing: then nothing is charged) under the key
//! `cancellation:<company_id>:<n>`, n = the company's requests so far + 1, so
//! a retry replays and a request after a withdrawal is a new one. The row in
//! `billing_cancellation_requests` stays `scheduled` until the subscription
//! really ends (`completed`, by the webhook item sync) or it is withdrawn.
//! Webhooks never withdraw: an out-of-order event would undo a real request.
//! The Managers and staff are emailed once per request and withdrawal, after
//! the company lock is released (best effort, logged with the company id).
//! `confirmation_sent_at` is set once the confirmation reached at least one
//! Manager (there is no per-recipient record, and resending to all would
//! repeat it to the ones who got it); while it is NULL the billing job resends
//! it after 10 minutes ([`resend_confirmations`]). Staff notices are not
//! resent. A second withdrawal is answered as done.
//!
//! The onboarding advance (Terms 11): a request ends a pending advance item
//! at once (deleted, the commitment suspended), so nothing is ever charged
//! in advance for a subscription that ends. A request made after the
//! advance was paid, while its credit is unused, ends the subscription at
//! the end of the month it paid for (`cancel_at`, no proration); one
//! already scheduled when the credit is granted is moved there
//! ([`end_with_prepaid_month`]). A company whose signup still waits for its
//! bank account's micro-deposits (no subscription yet) can request too: its
//! pending SetupIntent is cancelled and the signup never makes a
//! subscription ([`signup_cancelled`]); access then follows the bank window.
//! Stripe written first, then the rows: the webhook item sync reconciles a
//! request whose save was lost either way ([`reconcile_in`]).

use super::SubscriptionSnapshot;
use super::cancellation_emails::{
    ConfirmationInput, MOVED_STEP, MovedInput, OnboardingAdvance, StaffEvent, StaffNoticeInput,
    WithdrawalAdvance, WithdrawalInput, render_confirmation, render_moved, render_staff_email,
    render_staff_telegram, render_withdrawal,
};
use super::lock::{BUSY_MESSAGE, CompanyLock, LockError};
use super::reminder_emails::{RenderedEmail, SUPPORT_EMAIL};
use super::stripe::{StripeError, subscription_snapshot};
use super::{MANAGER_POSITION, payments_account_url};
use crate::onboarding_call::commitment::{
    Commitment, Resumption, Status, prepaid_cancel_at, resumption,
};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use chrono::{NaiveDateTime, SubsecRound};
use lambda_http::tracing;
use sqlx::{MySqlConnection, MySqlPool};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Longest reason kept (`billing_cancellation_requests.reason`).
pub const MAX_REASON_CHARS: usize = 1000;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long after a request the billing job resends a confirmation that
/// reached no Manager.
pub const CONFIRMATION_RETRY_AFTER_MINUTES: i64 = 10;
/// Confirmations the billing job resends per run.
pub const CONFIRMATIONS_PER_TICK: u32 = 20;

pub const LEGACY_MESSAGE: &str =
    "Your plan is managed by Granite Manager. To cancel, email support@granite-manager.com.";
pub const NO_SUBSCRIPTION_MESSAGE: &str = "There is no active subscription to cancel.";
pub const NOTHING_SCHEDULED_MESSAGE: &str = "There is no cancellation request to withdraw.";
pub const ALREADY_ENDED_MESSAGE: &str = "The cancellation has already taken effect.";
pub const REASON_TOO_LONG_MESSAGE: &str = "The reason can be at most 1000 characters.";
pub const BANK_JUST_VERIFIED_MESSAGE: &str = "Your bank account was just verified and the subscription is being set up. Try again in a minute.";

/// Stripe statuses of a subscription that is still running.
pub fn is_live_subscription_status(status: &str) -> bool {
    matches!(status, "trialing" | "active" | "past_due")
}

/// A `billing_cancellation_requests` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CancellationRequest {
    pub id: i64,
    pub requested_at: NaiveDateTime,
    /// When the subscription ends.
    pub effective_at: NaiveDateTime,
    /// `None` for a request found in Stripe (channel `stripe`).
    pub requested_by: Option<i32>,
    /// The requester's name (their email when they have none; empty when
    /// found in Stripe).
    pub requested_by_name: String,
    pub reason: Option<String>,
    /// Made while the signup waited for its bank account: the signup that
    /// will never make a subscription.
    pub signup_intent_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CancellationError {
    /// A bad request (the reason is too long).
    #[error("{0}")]
    Invalid(String),
    /// Not possible now; the message is for the user.
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Stripe(#[from] StripeError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// What a request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// A new request. `free_period`: made while trialing (nothing is charged).
    Created {
        request: CancellationRequest,
        free_period: bool,
    },
    /// One was already scheduled: nothing changed.
    Existing(CancellationRequest),
}

/// The reason trimmed; empty is none.
pub fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, CancellationError> {
    let Some(reason) = raw.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Ok(None);
    };
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(CancellationError::Invalid(REASON_TOO_LONG_MESSAGE.into()));
    }
    Ok(Some(reason.to_string()))
}

/// When a cancelled subscription ends: Stripe's `cancel_at`, else the trial
/// end while trialing, else the current period end.
pub fn end_date(
    stripe_cancel_at: Option<NaiveDateTime>,
    status: &str,
    trial_end: Option<NaiveDateTime>,
    period_end: Option<NaiveDateTime>,
) -> Option<NaiveDateTime> {
    stripe_cancel_at
        .or_else(|| trial_end.filter(|_| status == "trialing"))
        .or(period_end)
}

/// The next charge of a running subscription: the trial end while
/// trialing, else the current period end.
pub fn next_charge(
    status: &str,
    trial_end: Option<NaiveDateTime>,
    period_end: Option<NaiveDateTime>,
) -> Option<NaiveDateTime> {
    if status == "trialing" {
        trial_end.or(period_end)
    } else {
        period_end
    }
}

#[derive(Debug, sqlx::FromRow)]
struct BillingRow {
    plan_source: String,
    stripe_subscription_id: Option<String>,
    subscription_status: Option<String>,
    trial_end_at: Option<NaiveDateTime>,
    current_period_end: Option<NaiveDateTime>,
}

async fn billing_row(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<BillingRow>> {
    sqlx::query_as(
        "SELECT CAST(c.plan_source AS CHAR) AS plan_source, b.stripe_subscription_id,
                b.subscription_status, b.trial_end_at, b.current_period_end
           FROM company c LEFT JOIN company_billing b ON b.company_id = c.id
          WHERE c.id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The live subscription's id and status, or why there is none.
fn live_subscription(row: Option<&BillingRow>) -> Result<(String, String), CancellationError> {
    let Some(row) = row else {
        return Err(CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()));
    };
    if row.plan_source != "subscription" {
        return Err(CancellationError::Conflict(LEGACY_MESSAGE.into()));
    }
    match (&row.stripe_subscription_id, &row.subscription_status) {
        (Some(id), Some(status)) if is_live_subscription_status(status) => {
            Ok((id.clone(), status.clone()))
        }
        _ => Err(CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into())),
    }
}

const SELECT_REQUEST: &str = "SELECT r.id, r.requested_at, r.effective_at, r.requested_by,
        COALESCE(NULLIF(TRIM(u.name), ''), u.email, '') AS requested_by_name, r.reason,
        r.signup_intent_id
   FROM billing_cancellation_requests r LEFT JOIN users u ON u.id = r.requested_by";

/// The company's scheduled request, if any.
pub async fn scheduled(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<CancellationRequest>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_REQUEST} WHERE r.company_id = ? AND r.status = 'scheduled' ORDER BY r.id DESC LIMIT 1"
    )))
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

async fn by_id(pool: &MySqlPool, id: i64) -> sqlx::Result<CancellationRequest> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_REQUEST} WHERE r.id = ?"
    )))
    .bind(id)
    .fetch_one(pool)
    .await
}

/// A Stripe 400 for an idempotency key reused with other parameters (a
/// retry with another reason after a lost answer).
fn is_key_reuse(err: &StripeError) -> bool {
    matches!(err, StripeError::Api(failure)
        if failure.code == "idempotency_error"
            || failure.message.starts_with("Keys for idempotent requests"))
}

/// Requests cancellation for `user_id`'s company: schedules the Stripe
/// subscription to end, records the request and emails the Managers and
/// staff.
///
/// A scheduled request is answered as is, with no Stripe call and no
/// email.
pub async fn request(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    reason: Option<&str>,
) -> Result<Requested, CancellationError> {
    let reason = normalize_reason(reason)?;
    if scheduled(&state.pool, company_id).await?.is_none() {
        let row = billing_row(&state.pool, company_id).await?;
        if let Err(err) = live_subscription(row.as_ref())
            && awaiting_bank(&state.pool, row.as_ref(), company_id)
                .await?
                .is_none()
        {
            return Err(err);
        }
    }
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = request_locked(state, company_id, user_id, reason.as_deref()).await;
    lock.release().await;
    let outcome = result?;
    if let Requested::Created {
        request,
        free_period,
    } = &outcome
    {
        notify_requested(state, company_id, request, *free_period).await;
    }
    Ok(outcome)
}

async fn request_locked(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    reason: Option<&str>,
) -> Result<Requested, CancellationError> {
    let pool = &state.pool;
    if let Some(existing) = scheduled(pool, company_id).await? {
        return Ok(Requested::Existing(existing));
    }
    let row = billing_row(pool, company_id).await?;
    let (subscription_id, status) = match live_subscription(row.as_ref()) {
        Ok(live) => live,
        Err(err) => {
            return match awaiting_bank(pool, row.as_ref(), company_id).await? {
                Some(signup) => {
                    request_awaiting_bank(state, company_id, user_id, reason, &signup).await
                }
                None => Err(err),
            };
        }
    };
    let row = row.ok_or_else(|| CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()))?;
    let key = format!(
        "cancellation:{company_id}:{}",
        request_count(pool, company_id).await? + 1
    );
    let stripe = &state.stripe;
    // Read, not guessed: a lost read would end a prepaid month early.
    let commitment = crate::onboarding_call::store::load(pool, company_id)
        .await?
        .map(|stored| stored.commitment);
    // A paid advance whose credit is unused: the month it paid for is kept.
    let prepaid_end = commitment
        .as_ref()
        .and_then(|c| prepaid_cancel_at(c, super::now()));
    let subscription = subscription_id.as_str();
    let schedule = |key: String| async move {
        match prepaid_end {
            Some(end) => {
                stripe
                    .set_cancel_at(subscription, Some(end), reason, &key)
                    .await
            }
            None => {
                stripe
                    .set_cancel_at_period_end(subscription, true, reason, &key)
                    .await
            }
        }
    };
    let answer = match schedule(key.clone()).await {
        Ok(answer) => answer,
        Err(err) if is_key_reuse(&err) => {
            schedule(format!("{key}:{}", uuid::Uuid::new_v4())).await?
        }
        Err(StripeError::Missing(_)) => {
            return Err(CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()));
        }
        Err(err) => return Err(err.into()),
    };
    let snapshot = subscription_snapshot(&answer);
    let status = if snapshot.status.is_empty() {
        status
    } else {
        snapshot.status.clone()
    };
    let effective_at = end_date(
        snapshot.cancel_at.or(prepaid_end),
        &status,
        snapshot.trial_end.or(row.trial_end_at),
        snapshot.period_end.or(row.current_period_end),
    )
    .ok_or_else(|| StripeError::Request("Stripe did not say when the subscription ends".into()))?;
    let now = super::now();
    let mut tx = pool.begin().await?;
    let id = insert_request(
        &mut tx,
        company_id,
        Some(user_id),
        reason,
        effective_at,
        now,
        None,
    )
    .await?;
    // Optimistic: the subscription webhook mirrors Stripe's value anyway.
    sqlx::query("UPDATE company_billing SET cancel_at = ? WHERE company_id = ?")
        .bind(effective_at)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let request = by_id(pool, id).await?;
    withdraw_pending_advance(state, company_id).await;
    Ok(Requested::Created {
        request,
        free_period: status == "trialing",
    })
}

async fn request_count(pool: &MySqlPool, company_id: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM billing_cancellation_requests WHERE company_id = ?")
        .bind(company_id)
        .fetch_one(pool)
        .await
}

/// Inserts a scheduled request; its id.
async fn insert_request(
    conn: &mut MySqlConnection,
    company_id: i32,
    requested_by: Option<i32>,
    reason: Option<&str>,
    effective_at: NaiveDateTime,
    now: NaiveDateTime,
    origin: Option<RequestOrigin<'_>>,
) -> sqlx::Result<i64> {
    let (channel, signup) = match origin {
        None => ("app", None),
        Some(RequestOrigin::Stripe) => ("stripe", None),
        Some(RequestOrigin::Signup(id)) => ("app", Some(id)),
    };
    let id = sqlx::query(
        "INSERT INTO billing_cancellation_requests
           (company_id, requested_by, reason, effective_at, status, requested_at, channel,
            signup_intent_id)
         VALUES (?, ?, ?, ?, 'scheduled', ?, ?, ?)",
    )
    .bind(company_id)
    .bind(requested_by)
    .bind(reason)
    .bind(effective_at)
    .bind(now)
    .bind(channel)
    .bind(signup)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// Where a request not made by a Manager on a subscription came from.
#[derive(Debug, Clone, Copy)]
enum RequestOrigin<'a> {
    /// Stripe had the subscription set to end with no row here.
    Stripe,
    /// A Manager, while this signup waited for its bank account.
    Signup(&'a str),
}

/// The company's signup waiting for its bank account (no subscription yet),
/// for a self-serve company.
async fn awaiting_bank(
    pool: &MySqlPool,
    row: Option<&BillingRow>,
    company_id: i32,
) -> sqlx::Result<Option<crate::signup::intent::Intent>> {
    if row.is_none_or(|row| row.plan_source != "subscription") {
        return Ok(None);
    }
    crate::signup::provision::awaiting_signup(pool, company_id).await
}

/// U16: a request while the signup waits for micro-deposits. Its SetupIntent
/// is cancelled (keyed) and the request recorded with the signup, which then
/// never makes a subscription ([`signup_cancelled`]); it ends when the bank
/// window does (access pauses then). Nothing was or will be charged.
async fn request_awaiting_bank(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    reason: Option<&str>,
    signup: &crate::signup::intent::Intent,
) -> Result<Requested, CancellationError> {
    let pool = &state.pool;
    let n = request_count(pool, company_id).await? + 1;
    if let Some(setup_id) = signup.setup_intent_id.as_deref() {
        let current = match state.stripe.setup_intent(setup_id).await {
            Ok(setup) => setup["status"].as_str().map(str::to_string),
            Err(StripeError::Missing(_)) => None,
            Err(err) => return Err(err.into()),
        };
        match current.as_deref() {
            Some("succeeded") => {
                return Err(CancellationError::Conflict(
                    BANK_JUST_VERIFIED_MESSAGE.into(),
                ));
            }
            None | Some("canceled") => {}
            Some(_) => match state
                .stripe
                .cancel_setup_intent(
                    setup_id,
                    &format!("cancellation:{company_id}:{n}:setup-intent"),
                )
                .await
            {
                Ok(_) | Err(StripeError::Missing(_)) => {}
                Err(err) => return Err(err.into()),
            },
        }
    }
    let now = super::now();
    let effective_at = signup.bank_verification_until.unwrap_or(now).max(now);
    let mut tx = pool.begin().await?;
    let id = insert_request(
        &mut tx,
        company_id,
        Some(user_id),
        reason,
        effective_at,
        now,
        Some(RequestOrigin::Signup(&signup.id)),
    )
    .await?;
    tx.commit().await?;
    let request = by_id(pool, id).await?;
    Ok(Requested::Created {
        request,
        free_period: true,
    })
}

/// The signup has a scheduled cancellation request made while it waited for
/// its bank account: it must never make a subscription.
pub async fn signup_cancelled(pool: &MySqlPool, signup_id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM billing_cancellation_requests
                         WHERE signup_intent_id = ? AND status = 'scheduled') <> 0",
    )
    .bind(signup_id)
    .fetch_one(pool)
    .await
}

/// Right after a request is recorded: an advance item still pending is
/// deleted at once and the commitment suspended (the settlement reads the
/// request), so it can never be swept into an invoice; one Stripe already
/// put on an invoice is recorded and runs its course (credited once paid).
/// Best effort (the daily run retries); the caller holds the company lock.
async fn withdraw_pending_advance(state: &AppState, company_id: i32) {
    use crate::onboarding_call::{advance, commitment};
    let Ok(Some(c)) = load_commitment(state, company_id).await else {
        return;
    };
    if !commitment::item_open(&c) {
        return;
    }
    let until = Instant::now() + Duration::from_secs(15);
    if let Err(err) = advance::settle_item_locked(state, company_id, super::now(), until).await {
        tracing::warn!(company_id, code = %err.code(), "cancellation: advance item not settled");
    }
}

/// Moves a scheduled cancellation that ends before `until` (the end of the
/// month a paid advance covers) to `until`, without proration.
///
/// Then emails the Managers the new end date ([`notify_moved`]). The caller
/// holds the company lock. Nothing to do without a scheduled request.
pub async fn end_with_prepaid_month(
    state: &AppState,
    company_id: i32,
    until: NaiveDateTime,
    invoice_id: &str,
) -> Result<(), CancellationError> {
    let pool = &state.pool;
    let Some(existing) = scheduled(pool, company_id).await? else {
        return Ok(());
    };
    if existing.signup_intent_id.is_some() || existing.effective_at >= until {
        return Ok(());
    }
    let Ok((subscription_id, _)) = live_subscription(billing_row(pool, company_id).await?.as_ref())
    else {
        return Ok(());
    };
    state
        .stripe
        .set_cancel_at(
            &subscription_id,
            Some(until),
            None,
            &format!("onboarding-advance:{company_id}:{invoice_id}:cancel-at"),
        )
        .await?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE billing_cancellation_requests SET effective_at = ?
          WHERE id = ? AND status = 'scheduled'",
    )
    .bind(until)
    .bind(existing.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE company_billing SET cancel_at = ? WHERE company_id = ?")
        .bind(until)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(
        company_id,
        "cancellation moved to the end of the prepaid month"
    );
    notify_moved(state, company_id, &existing, until).await;
    Ok(())
}

/// Emails each Manager once that `request` now ends at `until`: claimed per
/// Manager and request in `billing_reminder_emails` before the send and
/// marked after it, so a replay never sends twice. Best effort.
async fn notify_moved(
    state: &AppState,
    company_id: i32,
    request: &CancellationRequest,
    until: NaiveDateTime,
) {
    use crate::onboarding_call::store;
    let pool = &state.pool;
    let Ok(managers) = store::managers(pool, company_id).await else {
        tracing::warn!(company_id, "cancellation email recipients not read");
        return;
    };
    let company = company_name(pool, company_id).await;
    let account_url = payments_account_url();
    let now = super::now();
    for manager in managers {
        let claim = store::claim_email(
            pool,
            company_id,
            manager.user_id,
            request.requested_at,
            MOVED_STEP,
            now,
        )
        .await;
        let Ok(Some(claim_id)) = claim else {
            if claim.is_err() {
                tracing::warn!(company_id, "cancellation moved email not claimed");
            }
            continue;
        };
        let email = render_moved(&MovedInput {
            company_name: &company,
            user_name: manager.name.as_deref(),
            previous_effective_at: request.effective_at,
            effective_at: until,
            account_url: &account_url,
        });
        let (status, code, sent_at) = match send_rendered(state, &manager.email, &email).await {
            Ok(()) => ("sent", None, Some(now)),
            Err(err) => {
                tracing::warn!(
                    company_id,
                    kind = failure_kind(&err),
                    "cancellation moved email failed"
                );
                ("failed", Some(failure_kind(&err)), None)
            }
        };
        if store::mark_email(pool, company_id, claim_id, status, code, None, sent_at)
            .await
            .is_err()
        {
            tracing::warn!(company_id, "cancellation moved email not recorded");
        }
    }
}

/// What [`reconcile_in`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciled {
    Nothing,
    /// Stripe ends the subscription with no request here: one was recorded
    /// (channel `stripe`, no requester).
    Recorded,
    /// Stripe renews with a request still scheduled here: it was withdrawn.
    Withdrawn,
}

/// M5: Stripe is written before the rows, so a crash or a database failure
/// between leaves them apart.
///
/// The webhook item sync (under the company lock,
/// `snapshot` read live from Stripe) makes the rows follow Stripe: an end set
/// with no scheduled request records one, a scheduled request with no end
/// set is withdrawn. The daily onboarding run then suspends or resumes the
/// commitment as for a Manager's request; an unsent confirmation is resent
/// by the billing job. A request made while the signup waited for its bank
/// account has no subscription to follow and is left alone.
pub async fn reconcile_in(
    tx: &mut MySqlConnection,
    company_id: i32,
    snapshot: &SubscriptionSnapshot,
    now: NaiveDateTime,
) -> sqlx::Result<Reconciled> {
    let current: Option<(i64, Option<String>)> = sqlx::query_as(
        "SELECT id, signup_intent_id FROM billing_cancellation_requests
          WHERE company_id = ? AND status = 'scheduled'
          ORDER BY id DESC LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    match (snapshot.cancel_at, current) {
        (Some(ends), None) => {
            insert_request(
                tx,
                company_id,
                None,
                None,
                ends,
                now,
                Some(RequestOrigin::Stripe),
            )
            .await?;
            Ok(Reconciled::Recorded)
        }
        (None, Some((id, None))) => {
            sqlx::query(
                "UPDATE billing_cancellation_requests
                    SET status = 'withdrawn', withdrawn_at = ?, withdrawn_by = NULL
                  WHERE id = ? AND status = 'scheduled'",
            )
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE company_billing SET cancel_at = NULL WHERE company_id = ?")
                .bind(company_id)
                .execute(&mut *tx)
                .await?;
            Ok(Reconciled::Withdrawn)
        }
        _ => Ok(Reconciled::Nothing),
    }
}

/// What a withdrawal did.
struct Withdrawn {
    request: CancellationRequest,
    at: NaiveDateTime,
    next_charge_at: Option<NaiveDateTime>,
}

/// Withdraws the company's scheduled request ("Keep my subscription"):
/// the Stripe subscription renews again, the row is `withdrawn`, and the
/// Managers and staff are emailed.
///
/// When the latest request is already
/// withdrawn, nothing is done (no Stripe call, no email).
pub async fn withdraw(
    state: &AppState,
    company_id: i32,
    user_id: i32,
) -> Result<(), CancellationError> {
    let row = billing_row(&state.pool, company_id).await?;
    if row
        .as_ref()
        .is_some_and(|row| row.plan_source != "subscription")
    {
        return Err(CancellationError::Conflict(LEGACY_MESSAGE.into()));
    }
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = withdraw_locked(state, company_id, user_id).await;
    lock.release().await;
    if let Some(withdrawn) = result? {
        notify_withdrawn(state, company_id, user_id, &withdrawn).await;
    }
    Ok(())
}

/// The status of the company's latest request, if it ever made one.
async fn latest_status(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT CAST(status AS CHAR) FROM billing_cancellation_requests
          WHERE company_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// `None`: the latest request is already withdrawn (a repeat), nothing done.
async fn withdraw_locked(
    state: &AppState,
    company_id: i32,
    user_id: i32,
) -> Result<Option<Withdrawn>, CancellationError> {
    let pool = &state.pool;
    let Some(existing) = scheduled(pool, company_id).await? else {
        if latest_status(pool, company_id).await?.as_deref() == Some("withdrawn") {
            return Ok(None);
        }
        return Err(CancellationError::Conflict(
            NOTHING_SCHEDULED_MESSAGE.into(),
        ));
    };
    let now = super::now();
    if existing.effective_at <= now {
        return Err(CancellationError::Conflict(ALREADY_ENDED_MESSAGE.into()));
    }
    if existing.signup_intent_id.is_some() {
        // Made while the signup waited for its bank account: no
        // subscription to renew. A new payment method resumes the signup.
        mark_withdrawn(pool, company_id, existing.id, user_id, now).await?;
        return Ok(Some(Withdrawn {
            request: existing,
            at: now,
            next_charge_at: None,
        }));
    }
    let row = billing_row(pool, company_id).await?;
    let (subscription_id, status) = live_subscription(row.as_ref())?;
    let row = row.ok_or_else(|| CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()))?;
    let key = format!("cancellation:{company_id}:{}:withdraw", existing.id);
    let stripe = &state.stripe;
    // An end at the prepaid month is a `cancel_at`, not the period-end flag.
    let at_set_date = match stripe.subscription(&subscription_id).await {
        Ok(live) => live["cancel_at_period_end"] != true && !live["cancel_at"].is_null(),
        Err(StripeError::Missing(_)) => {
            return Err(CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()));
        }
        Err(err) => return Err(err.into()),
    };
    let answer = if at_set_date {
        stripe
            .set_cancel_at(&subscription_id, None, None, &key)
            .await
    } else {
        stripe
            .set_cancel_at_period_end(&subscription_id, false, None, &key)
            .await
    };
    let answer = match answer {
        Ok(answer) => answer,
        Err(StripeError::Missing(_)) => {
            return Err(CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()));
        }
        Err(err) => return Err(err.into()),
    };
    let snapshot = subscription_snapshot(&answer);
    let status = if snapshot.status.is_empty() {
        status
    } else {
        snapshot.status.clone()
    };
    let next_charge_at = next_charge(
        &status,
        snapshot.trial_end.or(row.trial_end_at),
        snapshot.period_end.or(row.current_period_end),
    );
    mark_withdrawn(pool, company_id, existing.id, user_id, now).await?;
    Ok(Some(Withdrawn {
        request: existing,
        at: now,
        next_charge_at,
    }))
}

async fn mark_withdrawn(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
    user_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE billing_cancellation_requests
            SET status = 'withdrawn', withdrawn_at = ?, withdrawn_by = ?
          WHERE id = ? AND status = 'scheduled'",
    )
    .bind(now)
    .bind(user_id)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE company_billing SET cancel_at = NULL WHERE company_id = ?")
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// A Manager to email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagerContact {
    pub name: Option<String>,
    pub email: String,
}

/// The company's Managers (position 5 in the company, not deleted) with an
/// email, one per address.
pub(crate) async fn manager_contacts(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Vec<ManagerContact>> {
    let rows: Vec<(Option<String>, String)> = sqlx::query_as(
        "SELECT u.name, TRIM(u.email) FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND u.email IS NOT NULL AND TRIM(u.email) <> ''
            AND EXISTS (SELECT 1 FROM users_positions up
                         WHERE up.user_id = u.id AND up.company_id = u.company_id
                           AND up.position_id = ?)
          ORDER BY u.id",
    )
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_all(pool)
    .await?;
    let mut seen = HashSet::new();
    Ok(rows
        .into_iter()
        .filter(|(_, email)| seen.insert(email.to_lowercase()))
        .map(|(name, email)| ManagerContact { name, email })
        .collect())
}

pub(crate) async fn company_name(pool: &MySqlPool, company_id: i32) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT name FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_default()
}

/// The failure without its text, which can quote the recipient's address.
pub(crate) const fn failure_kind(err: &SendFailure) -> &'static str {
    match err {
        SendFailure::Rejected(_) => "rejected",
        SendFailure::Failed(_) => "failed",
    }
}

/// Sends one rendered system email (HTML and text) to `to`.
pub(crate) async fn send_rendered(
    state: &AppState,
    to: &str,
    email: &RenderedEmail,
) -> Result<(), SendFailure> {
    let message = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: super::from_address(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: email.subject.clone(),
        body: email.html.clone(),
        signature: None,
        reply_to: vec![SUPPORT_EMAIL.into()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    state
        .webhooks
        .send_notice(&message, &email.text, SEND_TIMEOUT)
        .await
        .map(|_| ())
}

async fn managers_or_log(state: &AppState, company_id: i32) -> Vec<ManagerContact> {
    manager_contacts(&state.pool, company_id)
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(company_id, "cancellation email recipients not read");
            Vec::new()
        })
}

async fn notify_staff(state: &AppState, notice: &StaffNoticeInput<'_>) {
    let company_id = notice.company_id;
    if let Some(staff) = state.signup.notify_user_id
        && !state
            .webhooks
            .notify_telegram(staff, &render_staff_telegram(notice))
            .await
    {
        tracing::warn!(company_id, "cancellation Telegram notice failed");
    }
    if let Some(to) = state.signup.notify_email.as_deref()
        && let Err(err) = send_rendered(state, to, &render_staff_email(notice)).await
    {
        tracing::warn!(
            company_id,
            kind = failure_kind(&err),
            "cancellation staff email failed"
        );
    }
}

/// Why a request made at `requested_at` avoids the onboarding advance
/// (Terms 11.6, as the daily run suspends).
///
/// Once its notice went out, before the charge date; before that any
/// request, before or after the deadline. `None` when no advance is pending
/// (done, waived, on an invoice, paid or void) or the charge date has come.
pub fn onboarding_advance(
    commitment: Option<&Commitment>,
    requested_at: NaiveDateTime,
) -> Option<OnboardingAdvance> {
    let commitment = commitment?;
    match (commitment.status, commitment.terms) {
        (Status::NoticeSent | Status::Suspended | Status::AdvanceScheduled, Some(terms))
            if commitment.notice_sent_at.is_some() =>
        {
            (commitment.stripe_invoice_id.is_none() && terms.charge_at > requested_at)
                .then_some(OnboardingAdvance::BeforeChargeDate(terms.charge_at))
        }
        (Status::Pending | Status::Booked | Status::Suspended, _) => {
            Some(if commitment.deadline_at > requested_at {
                OnboardingAdvance::BeforeDeadline(commitment.deadline_at)
            } else {
                OnboardingAdvance::BeforeNotice
            })
        }
        _ => None,
    }
}

/// The onboarding advance once a request made at `requested_at` is
/// withdrawn at `withdrawn_at` (Terms 11.6, as the daily run resumes:
/// [`resumption`]).
///
/// A notice sent stands while its charge is still at least 7 days away,
/// else a new notice comes first; with no notice the deadline decides.
/// `None` when no advance is pending or the request was made on or after
/// the charge date.
pub fn advance_after_withdrawal(
    commitment: Option<&Commitment>,
    requested_at: NaiveDateTime,
    withdrawn_at: NaiveDateTime,
) -> Option<WithdrawalAdvance> {
    let commitment = commitment?;
    let from_rule = || match resumption(commitment, withdrawn_at) {
        Resumption::Countdown => WithdrawalAdvance::Deadline(commitment.deadline_at),
        Resumption::KeepsCharge(at) => WithdrawalAdvance::ChargedOn(at),
        Resumption::NewNotice => WithdrawalAdvance::NewNotice,
    };
    match (commitment.status, commitment.terms) {
        (Status::NoticeSent | Status::Suspended | Status::AdvanceScheduled, Some(terms))
            if commitment.notice_sent_at.is_some() =>
        {
            (requested_at < terms.charge_at).then(from_rule)
        }
        (Status::Pending | Status::Booked | Status::Suspended, _) => Some(from_rule()),
        _ => None,
    }
}

/// The company's onboarding commitment, `None` without one; `Err` (logged)
/// when it cannot be read.
async fn load_commitment(state: &AppState, company_id: i32) -> Result<Option<Commitment>, ()> {
    crate::onboarding_call::store::load(&state.pool, company_id)
        .await
        .map(|stored| stored.map(|stored| stored.commitment))
        .map_err(|err| {
            tracing::warn!(company_id, error = %err, "cancellation: onboarding commitment not read");
        })
}

/// Emails the confirmation to every Manager; true when it reached at least
/// one (or the company has none to email), false when it must be resent.
async fn send_confirmation(
    state: &AppState,
    company_id: i32,
    company: &str,
    request: &CancellationRequest,
    free_period: bool,
) -> bool {
    let Ok(contacts) = manager_contacts(&state.pool, company_id).await else {
        tracing::warn!(company_id, "cancellation email recipients not read");
        return false;
    };
    let account_url = payments_account_url();
    let commitment = load_commitment(state, company_id).await.ok().flatten();
    let advance = onboarding_advance(commitment.as_ref(), request.requested_at);
    let mut sent = 0;
    for contact in &contacts {
        let email = render_confirmation(&ConfirmationInput {
            company_name: company,
            user_name: contact.name.as_deref(),
            requested_by: &request.requested_by_name,
            requested_at: request.requested_at,
            effective_at: request.effective_at,
            free_period,
            onboarding_advance: advance,
            account_url: &account_url,
        });
        match send_rendered(state, &contact.email, &email).await {
            Ok(()) => sent += 1,
            Err(err) => tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "cancellation confirmation email failed"
            ),
        }
    }
    sent > 0 || contacts.is_empty()
}

/// Records that the request's confirmation went out, unless already recorded.
async fn mark_confirmed(pool: &MySqlPool, id: i64, at: NaiveDateTime) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE billing_cancellation_requests SET confirmation_sent_at = ?
          WHERE id = ? AND confirmation_sent_at IS NULL",
    )
    .bind(at)
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn notify_withdrawn(state: &AppState, company_id: i32, user_id: i32, withdrawn: &Withdrawn) {
    let company = company_name(&state.pool, company_id).await;
    let by: String = sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(TRIM(name), ''), email, '') FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let account_url = payments_account_url();
    let onboarding_advance = match load_commitment(state, company_id).await {
        Ok(commitment) => advance_after_withdrawal(
            commitment.as_ref(),
            withdrawn.request.requested_at,
            withdrawn.at,
        ),
        Err(()) => Some(WithdrawalAdvance::Unknown),
    };
    for contact in managers_or_log(state, company_id).await {
        let email = render_withdrawal(&WithdrawalInput {
            company_name: &company,
            user_name: contact.name.as_deref(),
            withdrawn_by: &by,
            next_charge_at: withdrawn.next_charge_at,
            onboarding_advance,
            account_url: &account_url,
        });
        if let Err(err) = send_rendered(state, &contact.email, &email).await {
            tracing::warn!(
                company_id,
                kind = failure_kind(&err),
                "cancellation withdrawal email failed"
            );
        }
    }
    notify_staff(
        state,
        &StaffNoticeInput {
            event: StaffEvent::Withdrawn,
            company_name: &company,
            company_id,
            by: &by,
            effective_at: withdrawn.request.effective_at,
            reason: withdrawn.request.reason.as_deref(),
        },
    )
    .await;
}

async fn notify_requested(
    state: &AppState,
    company_id: i32,
    request: &CancellationRequest,
    free_period: bool,
) {
    let company = company_name(&state.pool, company_id).await;
    if send_confirmation(state, company_id, &company, request, free_period).await
        && let Err(err) = mark_confirmed(&state.pool, request.id, super::now()).await
    {
        tracing::warn!(company_id, error = %err, "cancellation confirmation not recorded");
    }
    notify_staff(
        state,
        &StaffNoticeInput {
            event: StaffEvent::Requested,
            company_name: &company,
            company_id,
            by: &request.requested_by_name,
            effective_at: request.effective_at,
            reason: request.reason.as_deref(),
        },
    )
    .await;
}

/// Whether a request was made during the free period, from the company's
/// trial end: it ends then, and was made before it.
pub fn made_in_free_period(
    trial_end: Option<NaiveDateTime>,
    requested_at: NaiveDateTime,
    effective_at: NaiveDateTime,
) -> bool {
    trial_end.is_some_and(|end| requested_at < end && effective_at <= end)
}

/// Resends, as of `now` (UTC), the confirmations of scheduled requests that
/// reached no Manager, made at least 10 minutes ago, up to
/// [`CONFIRMATIONS_PER_TICK`]; no new one is started after `deadline`.
///
/// Each is
/// sent under the company lock and recorded (`confirmation_sent_at`) only
/// once a Manager received it. Answers how many were resent. Errors are
/// logged with the company id only.
pub async fn resend_confirmations(
    state: &AppState,
    now: NaiveDateTime,
    deadline: Instant,
) -> usize {
    let pool = &state.pool;
    // A DATETIME keeps whole seconds.
    let now = now.trunc_subsecs(0);
    let due = now - chrono::Duration::minutes(CONFIRMATION_RETRY_AFTER_MINUTES);
    let rows: Vec<(i64, i32)> = match sqlx::query_as(
        "SELECT id, company_id FROM billing_cancellation_requests
          WHERE status = 'scheduled' AND confirmation_sent_at IS NULL AND requested_at <= ?
          ORDER BY id
          LIMIT ?",
    )
    .bind(due)
    .bind(CONFIRMATIONS_PER_TICK)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(error = %err, "cancellation confirmations not read");
            return 0;
        }
    };
    let mut resent = 0;
    for (id, company_id) in rows {
        if Instant::now() >= deadline {
            break;
        }
        match resend_one(state, id, company_id, now).await {
            Ok(true) => resent += 1,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(company_id, error = %err, "cancellation confirmation not resent");
            }
        }
    }
    resent
}

/// One request; true when its confirmation reached a Manager this time.
/// Under the company lock (so two runs never both send), the row is checked
/// again, the confirmation sent, and `confirmation_sent_at` set only after a
/// Manager received it: a crash before that leaves it to the next run.
async fn resend_one(
    state: &AppState,
    id: i64,
    company_id: i32,
    now: NaiveDateTime,
) -> Result<bool, CancellationError> {
    let lock = CompanyLock::acquire(&state.pool, company_id).await?;
    let result = resend_locked(state, id, company_id, now).await;
    lock.release().await;
    result
}

async fn resend_locked(
    state: &AppState,
    id: i64,
    company_id: i32,
    now: NaiveDateTime,
) -> Result<bool, CancellationError> {
    let pool = &state.pool;
    let pending: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM billing_cancellation_requests
          WHERE id = ? AND status = 'scheduled' AND confirmation_sent_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    if pending.is_none() {
        return Ok(false);
    }
    let (request, trial_end) = request_and_trial_end(pool, id, company_id).await?;
    let free_period = made_in_free_period(trial_end, request.requested_at, request.effective_at);
    let company = company_name(pool, company_id).await;
    if !send_confirmation(state, company_id, &company, &request, free_period).await {
        return Ok(false);
    }
    mark_confirmed(pool, id, now).await?;
    Ok(true)
}

async fn request_and_trial_end(
    pool: &MySqlPool,
    id: i64,
    company_id: i32,
) -> sqlx::Result<(CancellationRequest, Option<NaiveDateTime>)> {
    let request = by_id(pool, id).await?;
    let trial_end: Option<Option<NaiveDateTime>> =
        sqlx::query_scalar("SELECT trial_end_at FROM company_billing WHERE company_id = ?")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok((request, trial_end.flatten()))
}

impl From<CancellationError> for crate::error::ApiError {
    fn from(err: CancellationError) -> Self {
        match err {
            CancellationError::Invalid(message) => Self::BadRequest(message),
            CancellationError::Conflict(message) => Self::Conflict(message),
            CancellationError::Lock(LockError::Busy) => Self::Unavailable(BUSY_MESSAGE.into()),
            CancellationError::Lock(LockError::Database(err))
            | CancellationError::Database(err) => Self::Database(err),
            CancellationError::Stripe(StripeError::NotConfigured) => {
                Self::Unavailable("Billing is not available right now.".into())
            }
            CancellationError::Stripe(err) => {
                tracing::warn!(code = err.code(), "cancellation Stripe call failed");
                Self::Upstream("We could not reach the payment service. Please try again.".into())
            }
        }
    }
}

/// The company's scheduled cancellation request.
#[derive(Debug, serde::Serialize, crate::openapi::ToSchema)]
pub struct CancellationRequestBody {
    /// Request id.
    pub id: i64,
    /// When it was requested (ISO 8601 UTC).
    pub requested_at: chrono::DateTime<chrono::Utc>,
    /// When the subscription ends (ISO 8601 UTC): the end of the free period
    /// when requested during it (nothing is charged then), else the end of
    /// the current paid month. Everything keeps working until then.
    pub effective_at: chrono::DateTime<chrono::Utc>,
    /// Name of the Manager who requested it (their email when they have no
    /// name).
    pub requested_by: String,
    /// The reason given, or null.
    pub reason: Option<String>,
}

impl From<CancellationRequest> for CancellationRequestBody {
    fn from(request: CancellationRequest) -> Self {
        Self {
            id: request.id,
            requested_at: request.requested_at.and_utc(),
            effective_at: request.effective_at.and_utc(),
            requested_by: request.requested_by_name,
            reason: request.reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;
    use crate::onboarding_call::commitment::AdvanceTerms;

    fn commitment(status: Status) -> Commitment {
        Commitment {
            company_id: 1,
            status,
            created_at: at("2026-10-24 15:00:00"),
            deadline_at: at("2026-11-24 04:59:59"),
            original_deadline_at: at("2026-11-24 04:59:59"),
            tolled_days: 0,
            extension_used: false,
            signup_advance_cents: 30_000,
            booking: None,
            completed_at: None,
            completed_by: None,
            no_show_at: None,
            no_show_call_start_at: None,
            suspended_at: None,
            notice_sent_at: None,
            notice_cycle_at: None,
            terms: None,
            advance_item_id: None,
            stripe_invoice_id: None,
            advance_credit_id: None,
            review_hold_at: None,
            review_hold_by: None,
            availability_checked_on: None,
        }
    }

    /// The notice of 11-24 15:00: the advance goes on the 2027-01-24 charge.
    fn noticed(status: Status) -> Commitment {
        Commitment {
            notice_sent_at: Some(at("2026-11-24 15:00:00")),
            terms: Some(AdvanceTerms {
                advance_cents: 30_000,
                charge_at: at("2027-01-24 15:00:00"),
                prepaid_from: at("2027-02-24 15:00:00"),
                prepaid_until: at("2027-03-24 15:00:00"),
            }),
            ..commitment(status)
        }
    }

    #[test]
    fn the_advance_is_avoided_before_the_notice_or_after_it_before_the_charge() {
        let before = at("2026-11-01 12:00:00");
        let deadline = OnboardingAdvance::BeforeDeadline(at("2026-11-24 04:59:59"));
        for status in [Status::Pending, Status::Booked, Status::Suspended] {
            assert_eq!(
                onboarding_advance(Some(&commitment(status)), before),
                Some(deadline)
            );
            assert_eq!(
                onboarding_advance(Some(&commitment(status)), at("2026-11-25 12:00:00")),
                Some(OnboardingAdvance::BeforeNotice)
            );
        }
        let charge = OnboardingAdvance::BeforeChargeDate(at("2027-01-24 15:00:00"));
        for status in [
            Status::NoticeSent,
            Status::AdvanceScheduled,
            Status::Suspended,
        ] {
            let c = noticed(status);
            assert_eq!(
                onboarding_advance(Some(&c), at("2027-01-20 12:00:00")),
                Some(charge)
            );
            assert_eq!(
                onboarding_advance(Some(&c), at("2027-01-24 15:00:00")),
                None
            );
        }
        let invoiced = Commitment {
            stripe_invoice_id: Some("in_1".into()),
            ..noticed(Status::AdvanceScheduled)
        };
        assert_eq!(
            onboarding_advance(Some(&invoiced), at("2027-01-20 12:00:00")),
            None
        );
        for status in [
            Status::Completed,
            Status::Waived,
            Status::AdvancePaid,
            Status::AdvanceSkipped,
            Status::Void,
        ] {
            assert_eq!(onboarding_advance(Some(&commitment(status)), before), None);
        }
        assert_eq!(onboarding_advance(None, before), None);
    }

    #[test]
    fn a_withdrawal_seven_days_before_the_charge_keeps_it_and_a_later_one_gets_a_new_notice() {
        let asked = at("2026-12-01 12:00:00");
        for status in [Status::Suspended, Status::NoticeSent] {
            let c = noticed(status);
            assert_eq!(
                advance_after_withdrawal(Some(&c), asked, at("2027-01-17 15:00:00")),
                Some(WithdrawalAdvance::ChargedOn(at("2027-01-24 15:00:00")))
            );
            for withdrawn in [at("2027-01-17 15:00:01"), at("2027-02-20 12:00:00")] {
                assert_eq!(
                    advance_after_withdrawal(Some(&c), asked, withdrawn),
                    Some(WithdrawalAdvance::NewNotice)
                );
            }
        }
        // Asked on or after the charge date: the request changed nothing.
        assert_eq!(
            advance_after_withdrawal(
                Some(&noticed(Status::NoticeSent)),
                at("2027-01-24 15:00:00"),
                at("2027-01-25 12:00:00")
            ),
            None
        );
        for status in [Status::Pending, Status::Booked, Status::Suspended] {
            let c = commitment(status);
            assert_eq!(
                advance_after_withdrawal(Some(&c), asked, at("2026-11-20 12:00:00")),
                Some(WithdrawalAdvance::Deadline(at("2026-11-24 04:59:59")))
            );
            assert_eq!(
                advance_after_withdrawal(Some(&c), asked, at("2026-11-24 04:59:59")),
                Some(WithdrawalAdvance::NewNotice)
            );
        }
        for status in [Status::Completed, Status::AdvancePaid, Status::Void] {
            assert_eq!(
                advance_after_withdrawal(Some(&commitment(status)), asked, asked),
                None
            );
        }
        assert_eq!(advance_after_withdrawal(None, asked, asked), None);
    }

    #[test]
    fn reasons_are_trimmed_empty_is_none_and_long_is_refused() {
        assert_eq!(normalize_reason(None).unwrap(), None);
        assert_eq!(normalize_reason(Some("   ")).unwrap(), None);
        assert_eq!(
            normalize_reason(Some("  Too pricey \n")).unwrap(),
            Some("Too pricey".into())
        );
        let longest = "é".repeat(MAX_REASON_CHARS);
        assert_eq!(normalize_reason(Some(&longest)).unwrap(), Some(longest));
        assert!(matches!(
            normalize_reason(Some(&"x".repeat(MAX_REASON_CHARS + 1))),
            Err(CancellationError::Invalid(_))
        ));
    }

    #[test]
    fn the_end_date_is_stripes_else_the_trial_end_else_the_period_end() {
        let trial = Some(at("2027-01-21 05:00:00"));
        let period = Some(at("2026-11-21 05:00:00"));
        let stripe = Some(at("2026-12-01 00:00:00"));
        assert_eq!(end_date(stripe, "trialing", trial, period), stripe);
        assert_eq!(end_date(None, "trialing", trial, period), trial);
        assert_eq!(end_date(None, "active", trial, period), period);
        assert_eq!(end_date(None, "trialing", None, period), period);
        assert_eq!(end_date(None, "active", trial, None), None);
    }

    #[test]
    fn a_request_ending_at_the_trial_end_before_it_was_made_in_the_free_period() {
        let trial = Some(at("2027-01-21 05:00:00"));
        let asked = at("2026-12-01 12:00:00");
        assert!(made_in_free_period(trial, asked, at("2027-01-21 05:00:00")));
        assert!(!made_in_free_period(
            trial,
            asked,
            at("2027-02-21 05:00:00")
        ));
        assert!(!made_in_free_period(
            trial,
            at("2027-01-22 12:00:00"),
            at("2027-02-21 05:00:00")
        ));
        assert!(!made_in_free_period(None, asked, at("2027-01-21 05:00:00")));
    }

    #[test]
    fn the_next_charge_is_the_trial_end_while_trialing() {
        let trial = Some(at("2027-01-21 05:00:00"));
        let period = Some(at("2026-11-21 05:00:00"));
        assert_eq!(next_charge("trialing", trial, period), trial);
        assert_eq!(next_charge("active", trial, period), period);
        assert_eq!(next_charge("past_due", None, None), None);
    }

    #[test]
    fn only_running_subscriptions_can_be_cancelled() {
        for status in ["trialing", "active", "past_due"] {
            assert!(is_live_subscription_status(status));
        }
        for status in ["canceled", "incomplete", "unpaid", "paused", ""] {
            assert!(!is_live_subscription_status(status));
        }
        let row = |plan: &str, status: Option<&str>| BillingRow {
            plan_source: plan.into(),
            stripe_subscription_id: Some("sub_1".into()),
            subscription_status: status.map(str::to_string),
            trial_end_at: None,
            current_period_end: None,
        };
        assert_eq!(
            live_subscription(Some(&row("subscription", Some("active")))).unwrap(),
            ("sub_1".to_string(), "active".to_string())
        );
        for (plan, status, message) in [
            ("legacy", Some("active"), LEGACY_MESSAGE),
            ("subscription", Some("canceled"), NO_SUBSCRIPTION_MESSAGE),
            ("subscription", None, NO_SUBSCRIPTION_MESSAGE),
        ] {
            match live_subscription(Some(&row(plan, status))) {
                Err(CancellationError::Conflict(text)) => assert_eq!(text, message),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            live_subscription(None),
            Err(CancellationError::Conflict(_))
        ));
    }
}
