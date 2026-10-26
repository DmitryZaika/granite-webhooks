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

use super::cancellation_emails::{
    ConfirmationInput, OnboardingFee, StaffEvent, StaffNoticeInput, WithdrawalInput,
    render_confirmation, render_staff_email, render_staff_telegram, render_withdrawal,
};
use super::lock::{CompanyLock, LockError};
use super::reminder_emails::{RenderedEmail, SUPPORT_EMAIL};
use super::stripe::{StripeError, subscription_snapshot};
use super::{MANAGER_POSITION, payments_account_url};
use crate::onboarding_call::commitment::{Commitment, Status};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingEmail, SendFailure};
use chrono::{NaiveDateTime, SubsecRound};
use lambda_http::tracing;
use sqlx::MySqlPool;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Longest reason kept (`billing_cancellation_requests.reason`).
pub const MAX_REASON_CHARS: usize = 1000;
const FROM_ADDRESS: &str = "noreply@granite-manager.com";
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
    pub requested_by: i32,
    /// The requester's name (their email when they have none).
    pub requested_by_name: String,
    pub reason: Option<String>,
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
        COALESCE(NULLIF(TRIM(u.name), ''), u.email, '') AS requested_by_name, r.reason
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
/// staff. A scheduled request is answered as is, with no Stripe call and no
/// email.
pub async fn request(
    state: &AppState,
    company_id: i32,
    user_id: i32,
    reason: Option<&str>,
) -> Result<Requested, CancellationError> {
    let reason = normalize_reason(reason)?;
    if scheduled(&state.pool, company_id).await?.is_none() {
        live_subscription(billing_row(&state.pool, company_id).await?.as_ref())?;
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
    let (subscription_id, status) = live_subscription(row.as_ref())?;
    let row = row.ok_or_else(|| CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()))?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM billing_cancellation_requests WHERE company_id = ?",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    let key = format!("cancellation:{company_id}:{}", count + 1);
    let stripe = &state.stripe;
    let answer = match stripe
        .set_cancel_at_period_end(&subscription_id, true, reason, &key)
        .await
    {
        Ok(answer) => answer,
        Err(err) if is_key_reuse(&err) => {
            let fresh = format!("{key}:{}", uuid::Uuid::new_v4());
            stripe
                .set_cancel_at_period_end(&subscription_id, true, reason, &fresh)
                .await?
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
        snapshot.cancel_at,
        &status,
        snapshot.trial_end.or(row.trial_end_at),
        snapshot.period_end.or(row.current_period_end),
    )
    .ok_or_else(|| StripeError::Request("Stripe did not say when the subscription ends".into()))?;
    let now = super::now();
    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO billing_cancellation_requests
           (company_id, requested_by, reason, effective_at, status, requested_at)
         VALUES (?, ?, ?, ?, 'scheduled', ?)",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(reason)
    .bind(effective_at)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    // Optimistic: the subscription webhook mirrors Stripe's value anyway.
    sqlx::query("UPDATE company_billing SET cancel_at = ? WHERE company_id = ?")
        .bind(effective_at)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let request = by_id(pool, i64::try_from(id).unwrap_or(i64::MAX)).await?;
    Ok(Requested::Created {
        request,
        free_period: status == "trialing",
    })
}

/// What a withdrawal did.
struct Withdrawn {
    request: CancellationRequest,
    next_charge_at: Option<NaiveDateTime>,
}

/// Withdraws the company's scheduled request ("Keep my subscription"):
/// the Stripe subscription renews again, the row is `withdrawn`, and the
/// Managers and staff are emailed. When the latest request is already
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
    let row = billing_row(pool, company_id).await?;
    let (subscription_id, status) = live_subscription(row.as_ref())?;
    let row = row.ok_or_else(|| CancellationError::Conflict(NO_SUBSCRIPTION_MESSAGE.into()))?;
    let key = format!("cancellation:{company_id}:{}:withdraw", existing.id);
    let answer = match state
        .stripe
        .set_cancel_at_period_end(&subscription_id, false, None, &key)
        .await
    {
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
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE billing_cancellation_requests
            SET status = 'withdrawn', withdrawn_at = ?, withdrawn_by = ?
          WHERE id = ? AND status = 'scheduled'",
    )
    .bind(now)
    .bind(user_id)
    .bind(existing.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE company_billing SET cancel_at = NULL WHERE company_id = ?")
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(Withdrawn {
        request: existing,
        next_charge_at,
    }))
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
        from_address: FROM_ADDRESS.into(),
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

/// Why a request made at `requested_at` avoids the onboarding fee: before
/// the fee's charge date once its notice went out, else before the deadline.
/// `None` when no fee is pending (done, waived, already invoiced or void).
pub fn onboarding_fee(
    commitment: Option<&Commitment>,
    requested_at: NaiveDateTime,
) -> Option<OnboardingFee> {
    let commitment = commitment?;
    match commitment.status {
        Status::NoticeSent | Status::Suspended if commitment.notice_sent_at.is_some() => commitment
            .charge_after
            .filter(|charge| *charge > requested_at)
            .map(OnboardingFee::BeforeChargeDate),
        Status::Pending | Status::Booked | Status::Suspended => (commitment.deadline_at
            > requested_at)
            .then_some(OnboardingFee::BeforeDeadline(commitment.deadline_at)),
        _ => None,
    }
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
    let commitment = crate::onboarding_call::store::load(&state.pool, company_id)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(company_id, error = %err, "cancellation: onboarding commitment not read");
            None
        });
    let fee = onboarding_fee(
        commitment.as_ref().map(|stored| &stored.commitment),
        request.requested_at,
    );
    let mut sent = 0;
    for contact in &contacts {
        let email = render_confirmation(&ConfirmationInput {
            company_name: company,
            user_name: contact.name.as_deref(),
            requested_by: &request.requested_by_name,
            requested_at: request.requested_at,
            effective_at: request.effective_at,
            free_period,
            onboarding_fee: fee,
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
    for contact in managers_or_log(state, company_id).await {
        let email = render_withdrawal(&WithdrawalInput {
            company_name: &company,
            user_name: contact.name.as_deref(),
            withdrawn_by: &by,
            next_charge_at: withdrawn.next_charge_at,
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
/// [`CONFIRMATIONS_PER_TICK`]; no new one is started after `deadline`. Each is
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::billing::test_store::at;

    fn commitment(status: Status) -> Commitment {
        Commitment {
            company_id: 1,
            status,
            created_at: at("2026-10-24 15:00:00"),
            deadline_at: at("2026-11-24 04:59:59"),
            original_deadline_at: at("2026-11-24 04:59:59"),
            tolled_days: 0,
            extension_used: false,
            fee_cents: 60_000,
            booking: None,
            completed_at: None,
            completed_by: None,
            no_show_at: None,
            no_show_call_start_at: None,
            suspended_at: None,
            notice_sent_at: None,
            notice_cycle_at: None,
            charge_after: None,
            stripe_invoice_id: None,
            availability_checked_on: None,
        }
    }

    #[test]
    fn the_fee_is_avoided_before_the_deadline_or_after_the_notice_before_the_charge() {
        let before = at("2026-11-01 12:00:00");
        let deadline = OnboardingFee::BeforeDeadline(at("2026-11-24 04:59:59"));
        for status in [Status::Pending, Status::Booked, Status::Suspended] {
            assert_eq!(
                onboarding_fee(Some(&commitment(status)), before),
                Some(deadline)
            );
        }
        assert_eq!(
            onboarding_fee(
                Some(&commitment(Status::Pending)),
                at("2026-11-25 12:00:00")
            ),
            None
        );
        let noticed = Commitment {
            notice_sent_at: Some(at("2026-11-24 15:00:00")),
            charge_after: Some(at("2026-12-01 15:00:00")),
            ..commitment(Status::NoticeSent)
        };
        let in_notice_week = at("2026-11-27 12:00:00");
        assert_eq!(
            onboarding_fee(Some(&noticed), in_notice_week),
            Some(OnboardingFee::BeforeChargeDate(at("2026-12-01 15:00:00")))
        );
        assert_eq!(
            onboarding_fee(Some(&noticed), at("2026-12-02 12:00:00")),
            None
        );
        for status in [
            Status::Completed,
            Status::Waived,
            Status::FeeInvoiced,
            Status::FeePaid,
            Status::FeeFailed,
            Status::FeeSkipped,
            Status::Void,
        ] {
            assert_eq!(onboarding_fee(Some(&commitment(status)), before), None);
        }
        assert_eq!(onboarding_fee(None, before), None);
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
