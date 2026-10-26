//! `company_onboarding_commitments` with its timeline, the daily slot checks
//! (`onboarding_availability_days`), the reminder claims in
//! `billing_reminder_emails` and the facts the daily run reads.
//!
//! A commitment is written only at the version it was read at: a save that
//! finds another version is a [`SaveError::Conflict`] the caller handles.

use super::commitment::{Booking, Commitment, Event, Outcome, Status};
use crate::billing::MANAGER_POSITION;
use crate::billing::reminders::{
    REMINDER_MAX_ATTEMPTS, REMINDER_RETRY_AFTER_MINUTES, STUCK_SENDING_MINUTES,
};
use chrono::{Duration, NaiveDate, NaiveDateTime};
use serde_json::Value;
use sqlx::mysql::MySqlDatabaseError;
use sqlx::{FromRow, MySqlConnection, MySqlPool};

/// `billing_reminder_emails.cycle_kind` of the onboarding emails.
pub const EMAIL_CYCLE_KIND: &str = "onboarding";
/// `billing_reminder_emails.step` of the fee notice.
pub const NOTICE_STEP: &str = "fee_notice";
/// Users the outside-widget booking search asks Calendly about, at most.
pub const SEARCH_EMAILS: usize = 5;

/// A commitment and the version it was read at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub commitment: Commitment,
    pub version: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// Another write came first: read again and redo the change.
    #[error("the onboarding commitment changed meanwhile")]
    Conflict,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(FromRow)]
struct CommitmentRow {
    company_id: i32,
    status: String,
    deadline_at: NaiveDateTime,
    original_deadline_at: NaiveDateTime,
    tolled_days: i32,
    extension_used: bool,
    fee_cents: i32,
    calendly_invitee_uri: Option<String>,
    calendly_event_uri: Option<String>,
    calendly_reschedule_url: Option<String>,
    calendly_cancel_url: Option<String>,
    booked_start_at: Option<NaiveDateTime>,
    booked_end_at: Option<NaiveDateTime>,
    booked_at: Option<NaiveDateTime>,
    completed_at: Option<NaiveDateTime>,
    completed_by: Option<i32>,
    no_show_at: Option<NaiveDateTime>,
    no_show_call_start_at: Option<NaiveDateTime>,
    suspended_at: Option<NaiveDateTime>,
    notice_sent_at: Option<NaiveDateTime>,
    notice_cycle_at: Option<NaiveDateTime>,
    charge_after: Option<NaiveDateTime>,
    stripe_invoice_id: Option<String>,
    availability_checked_on: Option<NaiveDate>,
    version: i32,
    created_at: NaiveDateTime,
}

impl CommitmentRow {
    fn parse(self) -> sqlx::Result<Stored> {
        let status = Status::parse(&self.status)
            .ok_or_else(|| sqlx::Error::Protocol("unknown onboarding commitment status".into()))?;
        let booking = match (
            self.calendly_invitee_uri,
            self.calendly_event_uri,
            self.booked_start_at,
            self.booked_end_at,
            self.booked_at,
        ) {
            (Some(invitee_uri), Some(event_uri), Some(start_at), Some(end_at), Some(booked_at)) => {
                Some(Booking {
                    invitee_uri,
                    event_uri,
                    start_at,
                    end_at,
                    reschedule_url: self.calendly_reschedule_url,
                    cancel_url: self.calendly_cancel_url,
                    booked_at,
                })
            }
            _ => None,
        };
        Ok(Stored {
            commitment: Commitment {
                company_id: self.company_id,
                status,
                created_at: self.created_at,
                deadline_at: self.deadline_at,
                original_deadline_at: self.original_deadline_at,
                tolled_days: self.tolled_days,
                extension_used: self.extension_used,
                fee_cents: i64::from(self.fee_cents),
                booking,
                completed_at: self.completed_at,
                completed_by: self.completed_by,
                no_show_at: self.no_show_at,
                no_show_call_start_at: self.no_show_call_start_at,
                suspended_at: self.suspended_at,
                notice_sent_at: self.notice_sent_at,
                notice_cycle_at: self.notice_cycle_at,
                charge_after: self.charge_after,
                stripe_invoice_id: self.stripe_invoice_id,
                availability_checked_on: self.availability_checked_on,
            },
            version: self.version,
        })
    }
}

const COMMITMENT_SELECT: &str = "SELECT company_id, CAST(status AS CHAR) AS status, deadline_at,
        original_deadline_at, tolled_days, extension_used, fee_cents, calendly_invitee_uri,
        calendly_event_uri, calendly_reschedule_url, calendly_cancel_url, booked_start_at,
        booked_end_at, booked_at, completed_at, completed_by, no_show_at, no_show_call_start_at,
        suspended_at, notice_sent_at, notice_cycle_at, charge_after, stripe_invoice_id,
        availability_checked_on, version, created_at
   FROM company_onboarding_commitments";

/// The company's commitment; `None` for a company without one (legacy, or
/// moved onto a plan by staff).
pub async fn load(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Option<Stored>> {
    let row: Option<CommitmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{COMMITMENT_SELECT} WHERE company_id = ?"
    )))
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    row.map(CommitmentRow::parse).transpose()
}

async fn insert_events(
    conn: &mut MySqlConnection,
    company_id: i32,
    events: &[Event],
    now: NaiveDateTime,
) -> sqlx::Result<()> {
    for event in events {
        let detail = match &event.detail {
            Value::Null => None,
            detail => Some(detail.to_string()),
        };
        sqlx::query(
            "INSERT INTO company_onboarding_commitment_events
               (company_id, kind, from_status, to_status, deadline_at, actor_user_id, detail,
                created_at)
             VALUES (?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?)",
        )
        .bind(company_id)
        .bind(event.kind.as_str())
        .bind(event.from.map(Status::as_str))
        .bind(event.to.as_str())
        .bind(event.deadline_at)
        .bind(event.actor)
        .bind(detail)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The row provisioning makes (`commitment::new_commitment`), inside its
/// transaction. A company that already has one keeps it, and no events are
/// added then.
pub async fn insert(
    conn: &mut MySqlConnection,
    outcome: &Outcome,
    now: NaiveDateTime,
) -> sqlx::Result<bool> {
    let c = &outcome.commitment;
    let inserted = sqlx::query(
        "INSERT IGNORE INTO company_onboarding_commitments
           (company_id, status, deadline_at, original_deadline_at, tolled_days, extension_used,
            fee_cents, version, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(c.company_id)
    .bind(c.status.as_str())
    .bind(c.deadline_at)
    .bind(c.original_deadline_at)
    .bind(c.tolled_days)
    .bind(c.extension_used)
    .bind(c.fee_cents)
    .bind(c.created_at)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() != 1 {
        return Ok(false);
    }
    insert_events(conn, c.company_id, &outcome.events, now).await?;
    Ok(true)
}

/// Writes the outcome over the commitment read at `version`, with its events,
/// in one transaction. Returns the new version.
pub async fn save(
    pool: &MySqlPool,
    version: i32,
    outcome: &Outcome,
    now: NaiveDateTime,
) -> Result<i32, SaveError> {
    let c = &outcome.commitment;
    let booking = c.booking.as_ref();
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE company_onboarding_commitments
            SET status = ?, deadline_at = ?, original_deadline_at = ?, tolled_days = ?,
                extension_used = ?, fee_cents = ?, calendly_invitee_uri = ?,
                calendly_event_uri = ?, calendly_reschedule_url = ?, calendly_cancel_url = ?,
                booked_start_at = ?, booked_end_at = ?, booked_at = ?, completed_at = ?,
                completed_by = ?, no_show_at = ?, no_show_call_start_at = ?, suspended_at = ?,
                notice_sent_at = ?,
                notice_cycle_at = ?, charge_after = ?, stripe_invoice_id = ?,
                availability_checked_on = ?, updated_at = ?, version = version + 1
          WHERE company_id = ? AND version = ?",
    )
    .bind(c.status.as_str())
    .bind(c.deadline_at)
    .bind(c.original_deadline_at)
    .bind(c.tolled_days)
    .bind(c.extension_used)
    .bind(c.fee_cents)
    .bind(booking.map(|b| b.invitee_uri.as_str()))
    .bind(booking.map(|b| b.event_uri.as_str()))
    .bind(booking.and_then(|b| b.reschedule_url.as_deref()))
    .bind(booking.and_then(|b| b.cancel_url.as_deref()))
    .bind(booking.map(|b| b.start_at))
    .bind(booking.map(|b| b.end_at))
    .bind(booking.map(|b| b.booked_at))
    .bind(c.completed_at)
    .bind(c.completed_by)
    .bind(c.no_show_at)
    .bind(c.no_show_call_start_at)
    .bind(c.suspended_at)
    .bind(c.notice_sent_at)
    .bind(c.notice_cycle_at)
    .bind(c.charge_after)
    .bind(c.stripe_invoice_id.as_deref())
    .bind(c.availability_checked_on)
    .bind(now)
    .bind(c.company_id)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        return Err(SaveError::Conflict);
    }
    insert_events(&mut *tx, c.company_id, &outcome.events, now).await?;
    tx.commit().await?;
    Ok(version + 1)
}

/// Fee invoices recorded for the company so far (`invoiced` events): a new
/// invoice after a withdrawn one gets a new idempotency key.
pub async fn fee_invoices_recorded(pool: &MySqlPool, company_id: i32) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM company_onboarding_commitment_events
          WHERE company_id = ? AND kind = 'invoiced'",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await
}

/// The outcome changes something worth a write.
pub fn changes(stored: &Stored, outcome: &Outcome) -> bool {
    !outcome.events.is_empty() || outcome.commitment != stored.commitment
}

/// One timeline entry, with the acting superuser's name.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EventRow {
    pub id: i64,
    pub kind: String,
    pub from_status: Option<String>,
    pub to_status: String,
    pub deadline_at: NaiveDateTime,
    pub actor_user_id: Option<i32>,
    pub actor_name: Option<String>,
    /// JSON text.
    pub detail: Option<String>,
    pub created_at: NaiveDateTime,
}

/// The company's timeline, newest first (100).
pub async fn timeline(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<EventRow>> {
    sqlx::query_as(
        "SELECT e.id, e.kind, e.from_status, e.to_status, e.deadline_at, e.actor_user_id,
                u.name AS actor_name, CAST(e.detail AS CHAR) AS detail, e.created_at
           FROM company_onboarding_commitment_events e
           LEFT JOIN users u ON u.id = e.actor_user_id
          WHERE e.company_id = ?
          ORDER BY e.id DESC
          LIMIT 100",
    )
    .bind(company_id)
    .fetch_all(pool)
    .await
}

/// One day's slot check.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct AvailabilityDay {
    pub checked_on: NaiveDate,
    /// `available`, `no_slots`, `not_configured` or `error`.
    pub result: String,
    pub slots: i32,
}

/// A slot check to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAvailabilityDay {
    pub checked_on: NaiveDate,
    pub checked_at: NaiveDateTime,
    pub result: &'static str,
    pub slots: i32,
    pub first_slot_at: Option<NaiveDateTime>,
    pub window_start_at: NaiveDateTime,
    pub window_end_at: NaiveDateTime,
    pub event_type_uris: Vec<String>,
    pub error_code: Option<String>,
}

/// The day's slot check (one row per UTC day, shared by every company).
pub async fn availability_day(
    pool: &MySqlPool,
    day: NaiveDate,
) -> sqlx::Result<Option<AvailabilityDay>> {
    sqlx::query_as(
        "SELECT checked_on, result, slots FROM onboarding_availability_days WHERE checked_on = ?",
    )
    .bind(day)
    .fetch_optional(pool)
    .await
}

/// Records the day's check unless another run did first; true when this one
/// was recorded.
pub async fn insert_availability_day(
    pool: &MySqlPool,
    day: &NewAvailabilityDay,
) -> sqlx::Result<bool> {
    let uris = (!day.event_type_uris.is_empty())
        .then(|| serde_json::to_string(&day.event_type_uris).unwrap_or_default());
    let inserted = sqlx::query(
        "INSERT IGNORE INTO onboarding_availability_days
           (checked_on, checked_at, result, slots, first_slot_at, window_start_at, window_end_at,
            event_type_uris, error_code)
         VALUES (?, ?, ?, ?, ?, ?, ?, CAST(? AS JSON), ?)",
    )
    .bind(day.checked_on)
    .bind(day.checked_at)
    .bind(day.result)
    .bind(day.slots)
    .bind(day.first_slot_at)
    .bind(day.window_start_at)
    .bind(day.window_end_at)
    .bind(uris)
    .bind(day.error_code.as_deref())
    .execute(pool)
    .await?;
    Ok(inserted.rows_affected() == 1)
}

/// The slot checks recorded from `from` through `to`, oldest first.
pub async fn availability_days_between(
    pool: &MySqlPool,
    from: NaiveDate,
    to: NaiveDate,
) -> sqlx::Result<Vec<AvailabilityDay>> {
    sqlx::query_as(
        "SELECT checked_on, result, slots FROM onboarding_availability_days
          WHERE checked_on >= ? AND checked_on <= ?
          ORDER BY checked_on",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}

/// The slot checks of the 14 days up to `today`, newest first.
pub async fn recent_availability(
    pool: &MySqlPool,
    today: NaiveDate,
) -> sqlx::Result<Vec<AvailabilityDay>> {
    sqlx::query_as(
        "SELECT checked_on, result, slots FROM onboarding_availability_days
          WHERE checked_on > ? AND checked_on <= ?
          ORDER BY checked_on DESC
          LIMIT 14",
    )
    .bind(today - Duration::days(14))
    .bind(today)
    .fetch_all(pool)
    .await
}

/// MySQL error 1146: the table does not exist (yet).
fn is_missing_table(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db_error| db_error.try_downcast_ref::<MySqlDatabaseError>())
        .is_some_and(|db_error| db_error.number() == 1146)
}

/// `requested_at` of the company's latest cancellation request that is not
/// withdrawn (scheduled, or `completed` once the subscription ended, so a
/// completed request never resumes the countdown); `None` without one, or
/// before `billing_cancellation_requests` exists.
pub async fn cancellation_requested_at(
    pool: &MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<NaiveDateTime>> {
    let requested = sqlx::query_scalar(
        "SELECT requested_at FROM billing_cancellation_requests
          WHERE company_id = ? AND status <> 'withdrawn'
          ORDER BY requested_at DESC, id DESC
          LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await;
    match requested {
        Err(err) if is_missing_table(&err) => Ok(None),
        other => other,
    }
}

/// `requested_at` and `withdrawn_at` of the company's cancellation requests
/// withdrawn after `since` (the notice), oldest first; none before
/// `billing_cancellation_requests` exists.
pub async fn withdrawn_cancellations_since(
    pool: &MySqlPool,
    company_id: i32,
    since: NaiveDateTime,
) -> sqlx::Result<Vec<(NaiveDateTime, NaiveDateTime)>> {
    let withdrawn = sqlx::query_as(
        "SELECT requested_at, withdrawn_at FROM billing_cancellation_requests
          WHERE company_id = ? AND status = 'withdrawn' AND withdrawn_at > ?
          ORDER BY withdrawn_at, id",
    )
    .bind(company_id)
    .bind(since)
    .fetch_all(pool)
    .await;
    match withdrawn {
        Err(err) if is_missing_table(&err) => Ok(Vec::new()),
        other => other,
    }
}

/// The subscription is cancelled for real (`canceled`, or `incomplete_expired`:
/// it never started), or the company is deleted.
pub async fn subscription_ended(pool: &MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    let ended: Option<bool> = sqlx::query_scalar(
        "SELECT (c.deleted_at IS NOT NULL
                 OR COALESCE(cb.subscription_status IN ('canceled', 'incomplete_expired'), 0)) <> 0
           FROM company c
           LEFT JOIN company_billing cb ON cb.company_id = c.id
          WHERE c.id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(ended.unwrap_or(true))
}

/// The company accepted the `onboarding_fee` consent.
pub async fn consent_on_file(pool: &MySqlPool, company_id: i32) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM consent_records
                         WHERE company_id = ? AND kind = 'onboarding_fee') <> 0",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await
}

/// Someone an onboarding email goes to.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Contact {
    pub user_id: i32,
    pub name: Option<String>,
    pub email: String,
}

/// The company's Managers (position 5, not deleted) with an email, by id.
pub async fn managers(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<Contact>> {
    sqlx::query_as(
        "SELECT u.id AS user_id, u.name, TRIM(u.email) AS email
           FROM users u
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
    .await
}

/// Emails of the company's Managers and Admins (not deleted), for the
/// booking search outside the widget: at most [`SEARCH_EMAILS`], by user id.
pub async fn booker_emails(pool: &MySqlPool, company_id: i32) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT u.id, LOWER(TRIM(u.email))
           FROM users u
          WHERE u.company_id = ? AND u.is_deleted = 0
            AND u.email IS NOT NULL AND TRIM(u.email) <> ''
            AND (u.is_admin = 1
                 OR EXISTS (SELECT 1 FROM users_positions up
                             WHERE up.user_id = u.id AND up.company_id = u.company_id
                               AND up.position_id = ?))
          ORDER BY u.id
          LIMIT 20",
    )
    .bind(company_id)
    .bind(MANAGER_POSITION)
    .fetch_all(pool)
    .await?;
    let mut emails: Vec<String> = Vec::new();
    for (_, email) in rows {
        if !emails.contains(&email) && emails.len() < SEARCH_EMAILS {
            emails.push(email);
        }
    }
    Ok(emails)
}

fn is_duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
}

/// The send claim of one onboarding email to one user: the row is inserted
/// as `sending` before the email goes out (or a refused one with attempts
/// left is taken again). `None`: already sent, being sent, or not due again.
pub async fn claim_email(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
    cycle_started_at: NaiveDateTime,
    step: &str,
    now: NaiveDateTime,
) -> sqlx::Result<Option<i64>> {
    let inserted = sqlx::query(
        "INSERT INTO billing_reminder_emails
           (company_id, user_id, cycle_kind, cycle_started_at, step, status, attempts, claimed_at)
         VALUES (?, ?, ?, ?, ?, 'sending', 1, ?)",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(EMAIL_CYCLE_KIND)
    .bind(cycle_started_at)
    .bind(step)
    .bind(now)
    .execute(pool)
    .await;
    match inserted {
        Ok(done) => return Ok(i64::try_from(done.last_insert_id()).ok()),
        Err(err) if is_duplicate(&err) => {}
        Err(err) => return Err(err),
    }
    let retried = sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = 'sending', attempts = attempts + 1, claimed_at = ?, error_code = NULL
          WHERE company_id = ? AND user_id = ? AND cycle_kind = ? AND cycle_started_at = ?
            AND step = ? AND status = 'failed' AND attempts < ? AND claimed_at <= ?",
    )
    .bind(now)
    .bind(company_id)
    .bind(user_id)
    .bind(EMAIL_CYCLE_KIND)
    .bind(cycle_started_at)
    .bind(step)
    .bind(REMINDER_MAX_ATTEMPTS)
    .bind(now - Duration::minutes(REMINDER_RETRY_AFTER_MINUTES))
    .execute(pool)
    .await?;
    if retried.rows_affected() != 1 {
        return Ok(None);
    }
    sqlx::query_scalar(
        "SELECT id FROM billing_reminder_emails
          WHERE company_id = ? AND user_id = ? AND cycle_kind = ? AND cycle_started_at = ?
            AND step = ?",
    )
    .bind(company_id)
    .bind(user_id)
    .bind(EMAIL_CYCLE_KIND)
    .bind(cycle_started_at)
    .bind(step)
    .fetch_optional(pool)
    .await
}

/// How a claimed email went: `sent`, `failed` (refused, retried) or
/// `unknown` (never resent).
pub async fn mark_email(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
    status: &str,
    error_code: Option<&str>,
    message_id: Option<&str>,
    sent_at: Option<NaiveDateTime>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = ?, error_code = ?, ses_message_id = COALESCE(?, ses_message_id),
                sent_at = COALESCE(?, sent_at)
          WHERE id = ? AND company_id = ? AND status = 'sending'",
    )
    .bind(status)
    .bind(error_code)
    .bind(message_id)
    .bind(sent_at)
    .bind(id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// The company's onboarding emails still `sending` after 15 minutes (a crash
/// between send and record) become `unknown`, never resent.
pub async fn sweep_stuck_emails(
    pool: &MySqlPool,
    company_id: i32,
    now: NaiveDateTime,
) -> sqlx::Result<u64> {
    let swept = sqlx::query(
        "UPDATE billing_reminder_emails
            SET status = 'unknown', error_code = 'stuck_sending'
          WHERE company_id = ? AND cycle_kind = ? AND status = 'sending' AND claimed_at < ?",
    )
    .bind(company_id)
    .bind(EMAIL_CYCLE_KIND)
    .bind(now - Duration::minutes(STUCK_SENDING_MINUTES))
    .execute(pool)
    .await?;
    Ok(swept.rows_affected())
}

/// Companies the daily run may take now: an open commitment (or an invoice
/// to finish) whose per-company run (`onboarding-commitment:<id>`) is due.
pub async fn due_companies(
    pool: &MySqlPool,
    cutoff: NaiveDateTime,
    limit: u32,
) -> sqlx::Result<Vec<i32>> {
    sqlx::query_scalar(
        "SELECT oc.company_id
           FROM company_onboarding_commitments oc
           LEFT JOIN job_runs jr ON jr.job_key = CONCAT('onboarding-commitment:', oc.company_id)
          WHERE oc.status IN ('pending', 'booked', 'suspended', 'notice_sent', 'fee_invoiced')
            AND (jr.last_started_at IS NULL OR jr.last_started_at <= ?)
          ORDER BY oc.company_id
          LIMIT ?",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The company's name (empty when unnamed).
pub async fn company_name(pool: &MySqlPool, company_id: i32) -> sqlx::Result<String> {
    let name: Option<Option<String>> =
        sqlx::query_scalar("SELECT name FROM company WHERE id = ? LIMIT 1")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(name.flatten().unwrap_or_default())
}
