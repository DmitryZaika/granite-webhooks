//! The call log behind Recents (`phone_calls`), read per user: what is in
//! their Recents, seen and hidden (`phone_call_views`), and unread counts.
//!
//! A user's Recents: calls they placed or answered (`user_id`), every call of
//! a direct line assigned to them, and unanswered incoming calls of shared
//! and main lines that ring them; minus the calls they hid. A test call
//! (`POST /v1/phone/calls/test`, our number calling our own phone, so
//! `other_number = our_number`) is never in Recents or stats.
//!
//! Who may hear a call (play its recording, read its transcript and AI
//! note) is one rule everywhere, the app, the web phone and the CRM alike
//! ([`push_may_hear`]): the user who placed or answered it; the users of the
//! direct line it came in on or went out from; for an incoming call nobody
//! answered (missed, voicemail, AI message), the users its line rings (they
//! get its push); the owner of its deal; the company's admins (super admins
//! included), superusers and sales managers. Anyone else in the company
//! still sees that the call happened (when, who, how long), not what was
//! said.

use super::wire_enum;
use crate::auth::session::SessionUser;
use crate::openapi::ToSchema;
use crate::serde_helpers::{js_date, js_date_required};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};

/// `positions.id` of a sales manager (hears the company's calls).
pub const SALES_MANAGER_POSITION: i32 = 2;

/// Someone who asks to hear calls ([`push_may_hear`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hearer {
    pub user_id: i32,
    /// A company admin (super admins too) or superuser: hears every call of
    /// the company.
    pub admin: bool,
}

impl Hearer {
    pub const fn of(user: &SessionUser) -> Self {
        Self {
            user_id: user.id,
            admin: user.is_admin || user.is_superuser,
        }
    }
}

/// Appends whether `hearer` may hear call `pc` (the module docs' rule) as
/// one SQL condition.
pub fn push_may_hear(query: &mut QueryBuilder<MySql>, hearer: Hearer) {
    query
        .push("(")
        .push_bind(hearer.admin)
        .push(" OR pc.user_id <=> ")
        .push_bind(hearer.user_id)
        .push(
            " OR EXISTS (SELECT 1 FROM telnyx_number_users hnu
                           JOIN telnyx_numbers hn ON hn.id = hnu.number_id
                          WHERE hn.id = pc.number_id AND hn.company_id = pc.company_id
                            AND hnu.user_id = ",
        )
        .push_bind(hearer.user_id)
        .push(
            " AND (hn.kind = 'direct'
                                 OR (hnu.rings <> 0 AND pc.direction = 'inbound'
                                     AND pc.answered_at IS NULL)))
              OR EXISTS (SELECT 1 FROM deals hd
                           JOIN customers hc ON hc.id = hd.customer_id
                          WHERE hd.id = pc.deal_id AND hc.company_id = pc.company_id
                            AND hd.deleted_at IS NULL AND hd.user_id = ",
        )
        .push_bind(hearer.user_id)
        .push(
            ") OR EXISTS (SELECT 1 FROM users_positions hp
                           WHERE hp.company_id = pc.company_id AND hp.position_id = ",
        )
        .push_bind(SALES_MANAGER_POSITION)
        .push(" AND hp.user_id = ")
        .push_bind(hearer.user_id)
        .push("))");
}

/// Whether `hearer` may hear the company's call `call_id`; `None` when the
/// company has no such call.
pub async fn may_hear(
    pool: &MySqlPool,
    company_id: i32,
    hearer: Hearer,
    call_id: i64,
) -> sqlx::Result<Option<bool>> {
    let mut query = QueryBuilder::new("SELECT ");
    push_may_hear(&mut query, hearer);
    query
        .push(" AS may_hear FROM phone_calls pc WHERE pc.id = ")
        .push_bind(call_id)
        .push(" AND pc.company_id = ")
        .push_bind(company_id);
    query.build_query_scalar().fetch_optional(pool).await
}

wire_enum! {
    /// `inbound` (to the company) or `outbound` (from it).
    pub enum CallDirection {
        /// To one of the company's numbers.
        Inbound = "inbound",
        /// From the company to someone else.
        Outbound = "outbound",
    }
}

wire_enum! {
    /// Where a call stands.
    ///
    /// `ringing` and `in_progress` while live; then `answered`, `missed`
    /// (nobody answered), `declined`, `voicemail` or `ai_message` (the caller
    /// left one), `busy`, `no_answer`, `failed` or `canceled` (the caller hung
    /// up first).
    pub enum CallStatus {
        /// Ringing.
        Ringing = "ringing",
        /// Connected now.
        InProgress = "in_progress",
        /// Answered and over.
        Answered = "answered",
        /// Nobody answered.
        Missed = "missed",
        /// Declined.
        Declined = "declined",
        /// The caller left a voicemail.
        Voicemail = "voicemail",
        /// The AI Assistant took a message.
        AiMessage = "ai_message",
        /// The other side was busy.
        Busy = "busy",
        /// The other side did not answer.
        NoAnswer = "no_answer",
        /// The call failed.
        Failed = "failed",
        /// The caller hung up before an answer.
        Canceled = "canceled",
    }
}

wire_enum! {
    /// The AI note's progress: `none` (not recorded or AI notes off),
    /// `pending`, `processing`, `done`, `failed` or `skipped`.
    pub enum TranscriptStatus {
        /// Nothing to transcribe.
        None = "none",
        /// Waiting for the job.
        Pending = "pending",
        /// Being transcribed.
        Processing = "processing",
        /// The AI note is ready.
        Done = "done",
        /// Transcribing failed.
        Failed = "failed",
        /// Not transcribed (too short, no speech).
        Skipped = "skipped",
    }
}

wire_enum! {
    /// Which calls Recents lists: `all`, `missed` (incoming calls nobody
    /// answered, voicemails and AI messages included), `incoming`,
    /// `outgoing` or `recorded` (with a recording).
    pub enum CallFilter {
        /// Every call.
        All = "all",
        /// Unanswered incoming calls.
        Missed = "missed",
        /// Incoming calls.
        Incoming = "incoming",
        /// Outgoing calls.
        Outgoing = "outgoing",
        /// Calls with a recording.
        Recorded = "recorded",
    }
}

/// The customer of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CallCustomer {
    /// Customer id.
    pub id: i32,
    /// Name; empty when the customer has none.
    pub name: String,
    /// Company name; null when empty.
    pub company: Option<String>,
}

/// The company number of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OurNumber {
    /// E.164.
    pub e164: String,
    /// Its label; may be empty.
    pub label: String,
}

/// One call, as Recents and the call details show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallRow {
    /// Our call id.
    pub id: i64,
    /// `inbound` or `outbound`.
    pub direction: CallDirection,
    /// `ringing`, `in_progress`, `answered`, `missed`, `declined`,
    /// `voicemail`, `ai_message`, `busy`, `no_answer`, `failed` or
    /// `canceled`.
    pub status: CallStatus,
    /// When it started (RFC 3339).
    #[serde(serialize_with = "js_date_required")]
    pub started_at: DateTime<Utc>,
    /// When it was answered; null when never.
    #[serde(serialize_with = "js_date")]
    pub answered_at: Option<DateTime<Utc>>,
    /// When it ended; null while live.
    #[serde(serialize_with = "js_date")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Talk time in seconds, from answer to hang-up; null when never
    /// answered or still live.
    pub duration_secs: Option<i32>,
    /// The customer's (or other party's) number, E.164.
    pub other_number: String,
    /// The customer the number belongs to; null when unknown.
    pub customer: Option<CallCustomer>,
    /// The deal the call belongs to; null when none.
    pub deal_id: Option<i64>,
    /// The company number called or calling; null when unknown.
    pub our_number: Option<OurNumber>,
    /// Who placed or answered it; null when nobody did.
    pub user_id: Option<i32>,
    /// That user's name; null when nobody or no name.
    pub user_name: Option<String>,
    /// A recording can be played (`GET /v1/phone/calls/{id}/recording`):
    /// false also when the caller may not hear the call (only its user, the
    /// users of its direct line, for an unanswered incoming call the users
    /// its line rings, its deal's owner, admins and sales managers may).
    pub has_recording: bool,
    /// The AI note's progress.
    pub transcript_status: TranscriptStatus,
    /// The AI note; null until it is written, and for a caller who may not
    /// hear the call (see `hasRecording`).
    pub ai_note: Option<String>,
    /// The caller has seen it (calls they placed or answered always are).
    pub seen: bool,
    /// The carrier flagged the caller as possible spam.
    pub spam_likely: bool,
    /// Placed with "Call via my cell".
    pub via_cell: bool,
}

#[derive(FromRow)]
struct CallDbRow {
    id: i64,
    #[sqlx(try_from = "String")]
    direction: CallDirection,
    #[sqlx(try_from = "String")]
    status: CallStatus,
    started_at: DateTime<Utc>,
    answered_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    duration_secs: Option<i32>,
    other_number: String,
    customer_id: Option<i32>,
    customer_name: Option<String>,
    customer_company: Option<String>,
    deal_id: Option<i64>,
    our_e164: Option<String>,
    our_label: Option<String>,
    user_id: Option<i32>,
    user_name: Option<String>,
    has_recording: bool,
    #[sqlx(try_from = "String")]
    transcript_status: TranscriptStatus,
    ai_note: Option<String>,
    seen: bool,
    spam_likely: bool,
    via_cell: bool,
    /// The reader may hear it ([`push_may_hear`]).
    may_hear: bool,
}

fn trimmed(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

impl From<CallDbRow> for CallRow {
    fn from(row: CallDbRow) -> Self {
        Self {
            id: row.id,
            direction: row.direction,
            status: row.status,
            started_at: row.started_at,
            answered_at: row.answered_at,
            ended_at: row.ended_at,
            duration_secs: row.duration_secs,
            other_number: row.other_number,
            customer: row.customer_id.map(|id| CallCustomer {
                id,
                name: trimmed(row.customer_name).unwrap_or_default(),
                company: trimmed(row.customer_company),
            }),
            deal_id: row.deal_id,
            our_number: row.our_e164.map(|e164| OurNumber {
                e164,
                label: row.our_label.unwrap_or_default(),
            }),
            user_id: row.user_id,
            user_name: trimmed(row.user_name),
            has_recording: row.has_recording && row.may_hear,
            transcript_status: row.transcript_status,
            ai_note: row.ai_note.filter(|_| row.may_hear),
            seen: row.seen,
            spam_likely: row.spam_likely,
            via_cell: row.via_cell,
        }
    }
}

/// SQL: `pc` is not a test call.
pub const NOT_TEST_CALL: &str = "NOT (pc.other_number <=> pc.our_number)";

/// Calls as `CallRow`s for `hearer` (what they may hear of each), in the
/// company.
fn calls_for(hearer: Hearer, company_id: i32) -> QueryBuilder<MySql> {
    let me = hearer.user_id;
    let mut query = QueryBuilder::new(
        "SELECT CAST(pc.id AS SIGNED) AS id, CAST(pc.direction AS CHAR) AS direction,
                CAST(pc.status AS CHAR) AS status, pc.started_at, pc.answered_at, pc.ended_at,
                pc.duration_secs, pc.other_number, c.id AS customer_id, c.name AS customer_name,
                c.company_name AS customer_company, CAST(pc.deal_id AS SIGNED) AS deal_id,
                COALESCE(n.e164, pc.our_number) AS our_e164, n.label AS our_label,
                pc.user_id, u.name AS user_name,
                pc.recording_status = 'copied' AS has_recording,
                CAST(pc.transcript_status AS CHAR) AS transcript_status, pc.ai_note,
                (v.seen_at IS NOT NULL
                 OR ((pc.user_id <=> ",
    );
    query.push_bind(me).push(
        ") AND (pc.direction = 'outbound' OR pc.answered_at IS NOT NULL))) AS seen,
                pc.spam_likely <> 0 AS spam_likely, pc.via_cell <> 0 AS via_cell, ",
    );
    push_may_hear(&mut query, hearer);
    query
        .push(
            " AS may_hear
           FROM phone_calls pc
           LEFT JOIN customers c ON c.id = pc.customer_id AND c.company_id = pc.company_id
                                AND c.deleted_at IS NULL
           LEFT JOIN telnyx_numbers n ON n.id = pc.number_id AND n.company_id = pc.company_id
           LEFT JOIN users u ON u.id = pc.user_id
           LEFT JOIN phone_call_views v ON v.call_id = pc.id AND v.user_id = ",
        )
        .push_bind(me)
        .push(" WHERE pc.company_id = ")
        .push_bind(company_id);
    query
}

/// Appends the Recents rule for `me`: ` AND (` their call `)`, not hidden,
/// not a test call.
pub fn push_in_recents(query: &mut QueryBuilder<MySql>, me: i32) {
    query
        .push(" AND (pc.user_id = ")
        .push_bind(me)
        .push(
            " OR EXISTS (SELECT 1 FROM telnyx_number_users nu
                          JOIN telnyx_numbers tn ON tn.id = nu.number_id
                          JOIN users tu ON tu.id = nu.user_id AND tu.company_id = tn.company_id
                         WHERE nu.number_id = pc.number_id AND tn.company_id = pc.company_id
                           AND nu.user_id = ",
        )
        .push_bind(me)
        .push(
            " AND (tn.kind = 'direct'
                                OR (nu.rings <> 0 AND pc.direction = 'inbound'
                                    AND pc.answered_at IS NULL))))
              AND NOT EXISTS (SELECT 1 FROM phone_call_views hv
                               WHERE hv.call_id = pc.id AND hv.hidden_at IS NOT NULL AND hv.user_id = ",
        )
        .push_bind(me)
        .push(") AND ")
        .push(NOT_TEST_CALL);
}

/// SQL condition for a Recents filter.
const fn filter_sql(filter: CallFilter) -> &'static str {
    match filter {
        CallFilter::All => "TRUE",
        CallFilter::Missed => {
            "pc.direction = 'inbound' AND pc.answered_at IS NULL
             AND pc.status NOT IN ('ringing', 'in_progress', 'answered')"
        }
        CallFilter::Incoming => "pc.direction = 'inbound' AND pc.answered_at IS NOT NULL",
        CallFilter::Outgoing => "pc.direction = 'outbound'",
        CallFilter::Recorded => "pc.recording_status = 'copied'",
    }
}

/// What Recents lists.
#[derive(Debug, Clone, Copy)]
pub struct RecentsQuery<'a> {
    pub filter: CallFilter,
    /// Only calls with this number (last 10 digits).
    pub number_last10: Option<&'a str>,
    /// Only calls with ids below this (paging).
    pub before: Option<i64>,
    pub limit: usize,
}

/// A page of `me`'s Recents, newest first.
pub async fn recents(
    pool: &MySqlPool,
    company_id: i32,
    me: Hearer,
    page: RecentsQuery<'_>,
) -> sqlx::Result<Vec<CallRow>> {
    let mut query = calls_for(me, company_id);
    push_in_recents(&mut query, me.user_id);
    query.push(" AND ").push(filter_sql(page.filter));
    if let Some(last10) = page.number_last10 {
        query
            .push(" AND pc.other_last10 = ")
            .push_bind(last10.to_string());
    }
    if let Some(before) = page.before {
        query.push(" AND pc.id < ").push_bind(before);
    }
    query
        .push(" ORDER BY pc.id DESC LIMIT ")
        .push_bind(i64::try_from(page.limit).unwrap_or(i64::MAX));
    let rows: Vec<CallDbRow> = query.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().map(CallRow::from).collect())
}

/// One call of the company, as `me` sees it (in their Recents or not).
pub async fn call(
    pool: &MySqlPool,
    company_id: i32,
    me: Hearer,
    call_id: i64,
) -> sqlx::Result<Option<CallRow>> {
    let mut query = calls_for(me, company_id);
    query.push(" AND pc.id = ").push_bind(call_id);
    let row: Option<CallDbRow> = query.build_query_as().fetch_optional(pool).await?;
    Ok(row.map(CallRow::from))
}

/// The last `limit` calls with a customer (anyone's), newest first, as `me`
/// sees them.
pub async fn customer_calls(
    pool: &MySqlPool,
    company_id: i32,
    me: Hearer,
    customer_id: i32,
    limit: usize,
) -> sqlx::Result<Vec<CallRow>> {
    let mut query = calls_for(me, company_id);
    query
        .push(" AND pc.customer_id = ")
        .push_bind(customer_id)
        .push(" AND ")
        .push(NOT_TEST_CALL)
        .push(" ORDER BY pc.id DESC LIMIT ")
        .push_bind(i64::try_from(limit).unwrap_or(i64::MAX));
    let rows: Vec<CallDbRow> = query.build_query_as().fetch_all(pool).await?;
    Ok(rows.into_iter().map(CallRow::from).collect())
}

/// Unseen incoming calls of the last 30 days in a user's Recents, for the
/// badge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Unread {
    /// Unseen missed calls (nobody answered, no message left).
    pub missed: i64,
    /// Unseen voicemails.
    pub voicemail: i64,
    /// Unseen messages the AI Assistant took.
    pub ai_message: i64,
}

/// How far back unread calls count.
const UNREAD_DAYS: i64 = 30;

/// `me`'s unread counts (calls of the last 30 days).
pub async fn unread(pool: &MySqlPool, company_id: i32, me: i32) -> sqlx::Result<Unread> {
    let mut query = QueryBuilder::new(
        "SELECT CAST(COALESCE(SUM(pc.status IN ('missed', 'no_answer', 'canceled')), 0) AS SIGNED),
                CAST(COALESCE(SUM(pc.status = 'voicemail'), 0) AS SIGNED),
                CAST(COALESCE(SUM(pc.status = 'ai_message'), 0) AS SIGNED)
           FROM phone_calls pc
           LEFT JOIN phone_call_views v ON v.call_id = pc.id AND v.user_id = ",
    );
    query
        .push_bind(me)
        .push(
            " WHERE pc.direction = 'inbound' AND pc.answered_at IS NULL AND v.seen_at IS NULL
                AND pc.started_at >= UTC_TIMESTAMP(3) - INTERVAL ",
        )
        .push(UNREAD_DAYS)
        .push(" DAY AND pc.company_id = ")
        .push_bind(company_id);
    push_in_recents(&mut query, me);
    let (missed, voicemail, ai_message): (i64, i64, i64) =
        query.build_query_as().fetch_one(pool).await?;
    Ok(Unread {
        missed,
        voicemail,
        ai_message,
    })
}
