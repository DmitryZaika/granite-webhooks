//! Writes to the call log (`phone_calls`, `phone_call_legs`) as Telnyx
//! reports a call's progress.
//!
//! Every status change is conditional (it only
//! moves on from the statuses it may follow), so callbacks that arrive twice
//! or out of order cannot undo one another. Every write is scoped to the
//! company of the webhook token.
//!
//! Times and durations Telnyx reports (when a leg was answered or hung up,
//! how long it talked) are preferred to when we heard about them, and are
//! held to what is possible: never before the call started or after now,
//! talk time 0 to [`MAX_TALK_SECS`].

use super::calls::{CallDirection, CallStatus};
use super::lines::CallerId;
use super::numbers::last10;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, MySqlConnection, MySqlExecutor, MySqlPool};

/// The longest talk time a call may have (a day); longer is not believed.
pub const MAX_TALK_SECS: i64 = 86_400;

/// What the webhooks need to know of a call.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct CallFacts {
    pub id: i64,
    pub company_id: i32,
    /// Who placed or answered it.
    pub user_id: Option<i32>,
    #[sqlx(try_from = "String")]
    pub direction: CallDirection,
    #[sqlx(try_from = "String")]
    pub status: CallStatus,
    /// The customer's (other party's) number.
    pub other_number: String,
    pub customer_id: Option<i32>,
    pub deal_id: Option<i64>,
    pub number_id: Option<i32>,
    pub our_number: Option<String>,
    /// The caller ID an outgoing call shows.
    pub caller_id: Option<String>,
    pub via_cell: bool,
    pub answered: bool,
    /// The phone that answered (incoming calls).
    pub answered_device_id: Option<i64>,
    pub ended: bool,
    pub telnyx_call_sid: Option<String>,
    pub telnyx_call_control_id: Option<String>,
    pub started_at: DateTime<Utc>,
}

impl CallFacts {
    /// Our number calling our own phone (`POST /v1/phone/calls/test`).
    pub fn is_test(&self) -> bool {
        self.our_number.as_deref() == Some(self.other_number.as_str())
    }
}

const FACTS: &str = "SELECT CAST(id AS SIGNED) AS id, company_id, user_id,
        CAST(direction AS CHAR) AS direction, CAST(status AS CHAR) AS status, other_number,
        customer_id, CAST(deal_id AS SIGNED) AS deal_id, number_id, our_number, caller_id,
        via_cell <> 0 AS via_cell, answered_at IS NOT NULL AS answered,
        CAST(answered_device_id AS SIGNED) AS answered_device_id,
        ended_at IS NOT NULL AS ended, telnyx_call_sid, telnyx_call_control_id, started_at
   FROM phone_calls";

/// A call of the company.
pub async fn facts(
    pool: &MySqlPool,
    company_id: i32,
    call_id: i64,
) -> sqlx::Result<Option<CallFacts>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{FACTS} WHERE id = ? AND company_id = ?"
    )))
    .bind(call_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The call of the company with Telnyx's TeXML `CallSid`.
pub async fn by_call_sid(
    pool: &MySqlPool,
    company_id: i32,
    call_sid: &str,
) -> sqlx::Result<Option<CallFacts>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{FACTS} WHERE telnyx_call_sid = ? AND company_id = ?"
    )))
    .bind(call_sid)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// The outgoing app call of the company a Call Control event is about: by
/// its Call Control id (the app's leg), else by its call session (the
/// customer's leg shares it).
pub async fn by_call_control(
    pool: &MySqlPool,
    company_id: i32,
    call_control_id: Option<&str>,
    call_session_id: Option<&str>,
) -> sqlx::Result<Option<CallFacts>> {
    if call_control_id.is_none() && call_session_id.is_none() {
        return Ok(None);
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{FACTS} WHERE company_id = ? AND direction = 'outbound'
            AND (telnyx_call_control_id = ? OR telnyx_call_session_id = ?)
          ORDER BY id DESC LIMIT 1"
    )))
    .bind(company_id)
    .bind(call_control_id)
    .bind(call_session_id)
    .fetch_optional(pool)
    .await
}

/// A new incoming call.
#[derive(Debug, Clone)]
pub struct NewInbound<'a> {
    pub company_id: i32,
    pub number_id: i32,
    /// Our number, E.164.
    pub our_number: &'a str,
    /// The caller's number (E.164 when it is one).
    pub other_number: &'a str,
    pub other_last10: &'a str,
    pub customer_id: Option<i32>,
    pub deal_id: Option<i64>,
    pub call_sid: Option<&'a str>,
    /// `ringing`, or `missed` when nothing rings.
    pub status: CallStatus,
    pub devices_rung: usize,
    pub ai_notes: bool,
}

/// Logs a new incoming call; its id.
pub async fn insert_inbound(
    conn: &mut MySqlConnection,
    call: &NewInbound<'_>,
) -> sqlx::Result<i64> {
    let id = sqlx::query(
        "INSERT INTO phone_calls
           (company_id, direction, status, other_number, other_last10, customer_id, deal_id,
            number_id, our_number, telnyx_call_sid, started_at, devices_rung, ai_notes)
         VALUES (?, 'inbound', ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3), ?, ?)",
    )
    .bind(call.company_id)
    .bind(call.status.as_str())
    .bind(call.other_number)
    .bind(call.other_last10)
    .bind(call.customer_id)
    .bind(call.deal_id)
    .bind(call.number_id)
    .bind(call.our_number)
    .bind(call.call_sid)
    .bind(i32::try_from(call.devices_rung).unwrap_or(i32::MAX))
    .bind(call.ai_notes)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// A new outgoing call of the app (Call Control).
#[derive(Debug, Clone)]
pub struct NewOutbound<'a> {
    pub company_id: i32,
    pub user_id: Option<i32>,
    /// `ringing` (approved), or `failed` (refused: it ends at once).
    pub status: CallStatus,
    /// The number dialed (E.164, an N11 code, or as dialed).
    pub other_number: &'a str,
    pub customer_id: Option<i32>,
    pub deal_id: Option<i64>,
    /// The caller ID it goes out with.
    pub caller: Option<&'a CallerId>,
    /// The app's leg.
    pub call_control_id: Option<&'a str>,
    pub call_session_id: Option<&'a str>,
    pub ai_notes: bool,
}

/// A Telnyx id as stored (128 characters at most).
fn telnyx_id(id: &str) -> String {
    id.chars().take(128).collect()
}

/// Logs a new outgoing app call; its id. `executor` is the pool, or the
/// transaction that took the call's slot (`limits::take_call_slot`).
pub async fn insert_outbound<'e>(
    executor: impl MySqlExecutor<'e>,
    call: &NewOutbound<'_>,
) -> sqlx::Result<i64> {
    let other_number: String = call.other_number.chars().take(20).collect();
    let id = sqlx::query(
        "INSERT INTO phone_calls
           (company_id, user_id, direction, status, other_number, other_last10, customer_id,
            deal_id, number_id, our_number, caller_id, telnyx_call_control_id,
            telnyx_call_session_id, started_at, ended_at, ai_notes)
         VALUES (?, ?, 'outbound', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3),
                 IF(? = 'failed', UTC_TIMESTAMP(3), NULL), ?)",
    )
    .bind(call.company_id)
    .bind(call.user_id)
    .bind(call.status.as_str())
    .bind(&other_number)
    .bind(last10(&other_number).unwrap_or_default())
    .bind(call.customer_id)
    .bind(call.deal_id)
    .bind(call.caller.map(|caller| caller.number_id))
    .bind(call.caller.map(|caller| caller.e164.as_str()))
    .bind(call.caller.map(|caller| caller.e164.as_str()))
    .bind(call.call_control_id.map(telnyx_id))
    .bind(call.call_session_id.map(telnyx_id))
    .bind(call.status.as_str())
    .bind(call.ai_notes)
    .execute(executor)
    .await?
    .last_insert_id();
    Ok(i64::try_from(id).unwrap_or(i64::MAX))
}

/// A phone the call rings, before Telnyx says anything about its leg
/// (`queued`: it counts as ringing for declines).
pub async fn add_queued_leg(
    conn: &mut MySqlConnection,
    call_id: i64,
    device_id: i64,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO phone_call_legs (call_id, device_id, status) VALUES (?, ?, 'queued')")
        .bind(call_id)
        .bind(device_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// How far along a leg status is: a leg never goes back (a late `ringing`
/// does not undo `in-progress`, nor our `declined`).
fn leg_rank(status: &str) -> i32 {
    match status {
        "queued" => 0,
        "initiated" => 1,
        "ringing" => 2,
        "in-progress" | "answered" => 3,
        "declined" => 4,
        _ => 5,
    }
}

/// SQL: the rank of the stored `status` (as [`leg_rank`]).
const STORED_RANK: &str = "CASE status WHEN 'queued' THEN 0 WHEN 'initiated' THEN 1
     WHEN 'ringing' THEN 2 WHEN 'in-progress' THEN 3 WHEN 'answered' THEN 3
     WHEN 'declined' THEN 4 ELSE 5 END";

/// Records what Telnyx said about a leg (`CallSid` `leg_sid`) of the call:
/// the phone's queued leg gets its sid, a known leg moves forward, an
/// unknown one is added.
pub async fn record_leg(
    pool: &MySqlPool,
    call_id: i64,
    device_id: Option<i64>,
    leg_sid: &str,
    status: &str,
) -> sqlx::Result<()> {
    let status: String = status.chars().take(32).collect();
    let rank = leg_rank(&status);
    let known = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_call_legs SET status = IF(? >= {STORED_RANK}, ?, status)
          WHERE leg_call_sid = ? AND call_id = ?"
    )))
    .bind(rank)
    .bind(&status)
    .bind(leg_sid)
    .bind(call_id)
    .execute(pool)
    .await?
    .rows_affected();
    // sqlx counts matched rows: a known leg, whether its status moved or not.
    if known > 0 {
        return Ok(());
    }
    let queued = sqlx::query(
        "UPDATE phone_call_legs SET leg_call_sid = ?, status = ?
          WHERE call_id = ? AND device_id <=> ? AND leg_call_sid IS NULL
          ORDER BY id LIMIT 1",
    )
    .bind(leg_sid)
    .bind(&status)
    .bind(call_id)
    .bind(device_id)
    .execute(pool)
    .await?
    .rows_affected();
    if queued > 0 {
        return Ok(());
    }
    let inserted = sqlx::query(
        "INSERT INTO phone_call_legs (call_id, device_id, leg_call_sid, status) VALUES (?, ?, ?, ?)",
    )
    .bind(call_id)
    .bind(device_id)
    .bind(leg_sid)
    .bind(&status)
    .execute(pool)
    .await;
    match inserted {
        // The same leg reported twice at once: the other request saved it.
        Err(err)
            if err
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
        {
            Ok(())
        }
        other => other.map(drop),
    }
}

/// The phone (device) of a leg Telnyx named, when we know it.
pub async fn leg_device(
    pool: &MySqlPool,
    call_id: i64,
    leg_sid: &str,
) -> sqlx::Result<Option<i64>> {
    let device: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT CAST(device_id AS SIGNED) FROM phone_call_legs WHERE leg_call_sid = ? AND call_id = ?",
    )
    .bind(leg_sid)
    .bind(call_id)
    .fetch_optional(pool)
    .await?;
    Ok(device.flatten())
}

/// Moves the call to `to` when its status is one of `from`: whether it
/// moved.
pub async fn set_status(
    pool: &MySqlPool,
    company_id: i32,
    call_id: i64,
    from: &[CallStatus],
    to: CallStatus,
) -> sqlx::Result<bool> {
    if from.is_empty() {
        return Ok(false);
    }
    let mut query = sqlx::QueryBuilder::new("UPDATE phone_calls SET status = ");
    query
        .push_bind(to.as_str())
        .push(" WHERE id = ")
        .push_bind(call_id)
        .push(" AND company_id = ")
        .push_bind(company_id)
        .push(" AND status IN (");
    let mut list = query.separated(", ");
    for status in from {
        list.push_bind(status.as_str());
    }
    query.push(")");
    Ok(query.build().execute(pool).await?.rows_affected() > 0)
}

/// Who answered a call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Answer {
    /// The phone that answered (incoming calls).
    pub device_id: Option<i64>,
    /// The user who answered (the phone's, or the cell's owner).
    pub user_id: Option<i32>,
    /// That user's "AI notes" setting, for the recording.
    pub ai_notes: Option<bool>,
    /// When, as Telnyx says (else now); held between the call's start and
    /// now.
    pub at: Option<DateTime<Utc>>,
}

/// SQL: `{at}` (a bound time, or NULL for now) held between the call's
/// start and now.
const HELD_TIME: &str =
    "LEAST(GREATEST(COALESCE(?, UTC_TIMESTAMP(3)), started_at), UTC_TIMESTAMP(3))";

/// Marks the call answered (it is `in_progress` until it ends): the first
/// answer wins. Whether this one did.
pub async fn answered(
    pool: &MySqlPool,
    company_id: i32,
    call_id: i64,
    answer: Answer,
) -> sqlx::Result<bool> {
    let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_calls
            SET answered_at = {HELD_TIME},
                status = IF(status = 'ringing', 'in_progress', status),
                answered_device_id = COALESCE(?, answered_device_id),
                user_id = COALESCE(user_id, ?),
                ai_notes = COALESCE(?, ai_notes)
          WHERE id = ? AND company_id = ? AND answered_at IS NULL AND ended_at IS NULL"
    )))
    .bind(answer.at)
    .bind(answer.device_id)
    .bind(answer.user_id)
    .bind(answer.ai_notes)
    .bind(call_id)
    .bind(company_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed > 0)
}

/// A call that just ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    /// The call as it was before.
    pub before: CallFacts,
    /// Its final status.
    pub status: CallStatus,
    /// Talk time in seconds (answer to hang-up); `None` when never answered.
    pub talk_secs: Option<i32>,
}

/// How a call ended, as Telnyx reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct End {
    /// When it ended (Telnyx's event time); `None` = now. Held between the
    /// call's start and now.
    pub at: Option<DateTime<Utc>>,
    /// How long the answering leg talked, when Telnyx says
    /// (`DialCallDuration`, the leg's `CallDuration`); else answer to end.
    pub talk_secs: Option<i64>,
}

impl End {
    /// Ended at `at` (Telnyx's time, when it gave one).
    pub const fn at(at: Option<DateTime<Utc>>) -> Self {
        Self {
            at,
            talk_secs: None,
        }
    }
}

/// Ends the call once.
///
/// A live call (`ringing`, `in_progress`) becomes `answered` when it was
/// answered, else `unanswered`; other statuses (missed, voicemail ...)
/// stay. Talk time is Telnyx's when it gave one, else answer to hang-up,
/// 0 to [`MAX_TALK_SECS`]. `None` when the call had ended already (or is
/// not the company's).
pub async fn finalize(
    pool: &MySqlPool,
    company_id: i32,
    call_id: i64,
    unanswered: CallStatus,
    hangup_cause: Option<&str>,
    end: End,
) -> sqlx::Result<Option<Ended>> {
    let mut tx = pool.begin().await?;
    let before: Option<CallFacts> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{FACTS} WHERE id = ? AND company_id = ? AND ended_at IS NULL FOR UPDATE"
    )))
    .bind(call_id)
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(before) = before else {
        tx.rollback().await?;
        return Ok(None);
    };
    let status = match before.status {
        CallStatus::Ringing | CallStatus::InProgress if before.answered => CallStatus::Answered,
        CallStatus::Ringing | CallStatus::InProgress => unanswered,
        other => other,
    };
    let cause: Option<String> = hangup_cause
        .map(str::trim)
        .filter(|cause| !cause.is_empty())
        .map(|cause| cause.chars().take(64).collect());
    // MySQL assigns left to right: `duration_secs` reads the new `ended_at`.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_calls
            SET status = ?, ended_at = {HELD_TIME},
                duration_secs = CASE WHEN answered_at IS NOT NULL
                    THEN LEAST({MAX_TALK_SECS}, GREATEST(0,
                           COALESCE(?, TIMESTAMPDIFF(SECOND, answered_at, ended_at)))) END,
                hangup_cause = COALESCE(?, hangup_cause), on_hold = 0
          WHERE id = ?"
    )))
    .bind(status.as_str())
    .bind(end.at)
    .bind(end.talk_secs.map(|secs| secs.clamp(0, MAX_TALK_SECS)))
    .bind(cause)
    .bind(call_id)
    .execute(&mut *tx)
    .await?;
    let talk_secs: Option<i32> =
        sqlx::query_scalar("SELECT duration_secs FROM phone_calls WHERE id = ?")
            .bind(call_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Some(Ended {
        before,
        status,
        talk_secs,
    }))
}

/// A customer's oldest open deal (the deal a call with them belongs to).
pub async fn open_deal(pool: &MySqlPool, customer_id: Option<i32>) -> sqlx::Result<Option<i64>> {
    let Some(customer_id) = customer_id else {
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT CAST(MIN(id) AS SIGNED) FROM deals
          WHERE customer_id = ? AND deleted_at IS NULL AND is_won IS NULL",
    )
    .bind(customer_id)
    .fetch_one(pool)
    .await
}

/// A customer's name for text to speech (trimmed; `None` when empty or not
/// the company's).
pub async fn customer_name(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: Option<i32>,
) -> sqlx::Result<Option<String>> {
    let Some(customer_id) = customer_id else {
        return Ok(None);
    };
    let name: Option<Option<String>> = sqlx::query_scalar(
        "SELECT name FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(customer_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(name
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty()))
}
