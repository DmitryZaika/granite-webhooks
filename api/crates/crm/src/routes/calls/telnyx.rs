//! Granite Phone (Telnyx) calls in the CRM's call panels: our own call log
//! (`phone_calls`) read with the session cookie, in the `CloudTalk` call
//! shape the panels read (as `RingCentral`'s calls are, see
//! [`super::ringcentral`]), plus a `Telnyx` part with what only our log has
//! (status, line, AI note).
//!
//! In the CRM a Granite Phone call is `tx_<call id>` (call lists,
//! recordings, transcripts, call coaching), so it never collides with a
//! `CloudTalk` call id.
//!
//! Every employee of the company sees its calls in the panels (as with
//! `CloudTalk`), but its recording, transcript and AI note only those who
//! may hear it, the app's rule (`crate::phone::calls::push_may_hear`): the
//! call's user, the users of its direct line, for an unanswered incoming
//! call the users its line rings, its deal's owner, admins and sales
//! managers.

use super::recording::CallTranscriptError;
use super::ringcentral::empty_agent;
use crate::ai::Caller;
use crate::phone::calls::{
    CallDirection, CallStatus, Hearer, NOT_TEST_CALL, TranscriptStatus, push_may_hear,
};
use crate::state::AppState;
use crate::transcribe::Transcript;
use chrono::{DateTime, SecondsFormat, Utc};
use lambda_http::tracing;
use serde_json::{Value, json};
use sqlx::{FromRow, MySql, MySqlPool, QueryBuilder};

/// What a Granite Phone call id starts with in the CRM.
pub const TOKEN_PREFIX: &str = "tx_";

/// The CRM's id of our call `id`: `tx_<id>`.
pub fn token(id: i64) -> String {
    format!("{TOKEN_PREFIX}{id}")
}

/// Our call id in a CRM id (`tx_<digits>`). Strict: these come back from
/// the browser.
pub fn parse_token(value: &str) -> Option<i64> {
    let digits = value.trim().strip_prefix(TOKEN_PREFIX)?;
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|id| *id > 0)
}

/// Which calls [`list`] reads: ended calls of the company (never test
/// calls), newest first.
#[derive(Debug, Default)]
pub struct Filter<'a> {
    /// With one of these numbers (last 10 digits) ...
    pub numbers: &'a [String],
    /// ... or logged to this customer.
    pub customer_id: Option<i32>,
    /// Instead of numbers: the calls of these users (placed or answered by
    /// them, and unanswered calls of their direct lines).
    pub users: &'a [i32],
    /// Only calls someone answered.
    pub answered_only: bool,
    /// Started at or after this.
    pub from: Option<DateTime<Utc>>,
    /// Started before this.
    pub to: Option<DateTime<Utc>>,
    /// At most this many.
    pub limit: i64,
}

/// One call of the call log, as the CRM shows it.
#[derive(Debug, Clone, FromRow)]
pub struct TelnyxCall {
    pub id: i64,
    #[sqlx(try_from = "String")]
    pub direction: CallDirection,
    #[sqlx(try_from = "String")]
    pub status: CallStatus,
    pub started_at: DateTime<Utc>,
    pub answered_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Talk time in seconds; `None` when never answered.
    pub duration_secs: Option<i32>,
    /// The customer's (other party's) number.
    pub other_number: String,
    pub customer_id: Option<i32>,
    pub customer_name: Option<String>,
    pub deal_id: Option<i64>,
    pub number_id: Option<i32>,
    /// Our number, E.164.
    pub our_number: Option<String>,
    /// Our number's label; `None` when it has none.
    pub line_label: Option<String>,
    /// Who placed or answered it.
    pub user_id: Option<i32>,
    pub user_name: Option<String>,
    /// With [`Filter::users`]: the user it is listed for (who handled it,
    /// else the owner of the direct line it rang).
    pub owner_id: Option<i32>,
    /// Its recording is in our bucket.
    pub recorded: bool,
    #[sqlx(try_from = "String")]
    pub transcript_status: TranscriptStatus,
    pub ai_note: Option<String>,
    /// The AI note was added to the deal.
    pub note_added: bool,
    pub via_cell: bool,
    pub spam_likely: bool,
    /// The reader may hear it: play its recording, read its transcript and
    /// AI note (`push_may_hear`).
    pub may_hear: bool,
}

/// The calls `filter` names, newest first, as `hearer` may see them
/// ([`TelnyxCall::may_hear`]).
pub async fn list(
    pool: &MySqlPool,
    company_id: i32,
    hearer: Hearer,
    filter: &Filter<'_>,
) -> sqlx::Result<Vec<TelnyxCall>> {
    let by_users = !filter.users.is_empty();
    if !by_users && filter.numbers.is_empty() && filter.customer_id.is_none() {
        return Ok(Vec::new());
    }
    let owner = if by_users {
        "COALESCE(pc.user_id, dnu.user_id)"
    } else {
        "pc.user_id"
    };
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT CAST(pc.id AS SIGNED) AS id, CAST(pc.direction AS CHAR) AS direction,
                CAST(pc.status AS CHAR) AS status, pc.started_at, pc.answered_at, pc.ended_at,
                pc.duration_secs, pc.other_number, c.id AS customer_id, c.name AS customer_name,
                CAST(pc.deal_id AS SIGNED) AS deal_id, pc.number_id,
                COALESCE(n.e164, pc.our_number) AS our_number,
                NULLIF(TRIM(n.label), '') AS line_label, pc.user_id, u.name AS user_name,
                {owner} AS owner_id, pc.recording_status = 'copied' AS recorded,
                CAST(pc.transcript_status AS CHAR) AS transcript_status, pc.ai_note,
                pc.note_id IS NOT NULL AS note_added, pc.via_cell <> 0 AS via_cell,
                pc.spam_likely <> 0 AS spam_likely, "
    ));
    push_may_hear(&mut query, hearer);
    query.push(
        " AS may_hear
           FROM phone_calls pc
           LEFT JOIN customers c ON c.id = pc.customer_id AND c.company_id = pc.company_id
                                AND c.deleted_at IS NULL
           LEFT JOIN telnyx_numbers n ON n.id = pc.number_id AND n.company_id = pc.company_id
           LEFT JOIN users u ON u.id = pc.user_id",
    );
    if by_users {
        query.push(
            " LEFT JOIN telnyx_numbers dn ON dn.id = pc.number_id AND dn.company_id = pc.company_id
                                         AND dn.kind = 'direct' AND pc.user_id IS NULL
              LEFT JOIN telnyx_number_users dnu ON dnu.number_id = dn.id",
        );
    }
    query
        .push(" WHERE pc.company_id = ")
        .push_bind(company_id)
        .push(" AND pc.ended_at IS NOT NULL AND ")
        .push(NOT_TEST_CALL);
    if by_users {
        query.push(format!(" AND {owner} IN ("));
        let mut users = query.separated(", ");
        for user_id in filter.users {
            users.push_bind(*user_id);
        }
        query.push(")");
    } else {
        query.push(" AND (");
        if !filter.numbers.is_empty() {
            query.push("pc.other_last10 IN (");
            let mut numbers = query.separated(", ");
            for number in filter.numbers {
                numbers.push_bind(number.clone());
            }
            query.push(")");
        }
        if let Some(customer_id) = filter.customer_id {
            if !filter.numbers.is_empty() {
                query.push(" OR ");
            }
            query.push("pc.customer_id = ").push_bind(customer_id);
        }
        query.push(")");
    }
    if filter.answered_only {
        query.push(" AND pc.answered_at IS NOT NULL");
    }
    if let Some(from) = filter.from {
        query.push(" AND pc.started_at >= ").push_bind(from);
    }
    if let Some(to) = filter.to {
        query.push(" AND pc.started_at < ").push_bind(to);
    }
    query
        .push(" ORDER BY pc.started_at DESC, pc.id DESC LIMIT ")
        .push_bind(filter.limit.max(1));
    query.build_query_as().fetch_all(pool).await
}

/// RFC 3339 in UTC, as the browser reads it.
pub fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The call in the `CloudTalk` shape of `GET /v1/calls/{provider}`, plus a
/// `Telnyx` part: our `call_id`, `status`, `line_label` and `our_number`,
/// `customer_id`, `deal_id`, `transcript_status`, `ai_note`, `note_added`
/// (the AI note is on the deal), `via_cell`, `spam_likely` and `may_hear`.
/// For a reader who may not hear the call it shows no recording
/// (`recorded` false, no `recording_link`) and no AI note.
pub fn item(call: &TelnyxCall) -> Value {
    let id = token(call.id);
    let recorded = call.recorded && call.may_hear;
    let ai_note = call.ai_note.as_ref().filter(|_| call.may_hear);
    let talk = call.duration_secs.unwrap_or(0).max(0).to_string();
    let kind = match call.direction {
        CallDirection::Inbound => "incoming",
        CallDirection::Outbound => "outgoing",
    };
    let our_number = call.our_number.clone().unwrap_or_default();
    let label = call.line_label.clone().unwrap_or_default();
    let user_name = call
        .user_name
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let mut agent = empty_agent();
    agent["id"] = json!(call.user_id.unwrap_or(0));
    agent["firstname"] = json!(user_name);
    agent["fullname"] = json!(user_name);
    json!({
        "Cdr": {
            "id": id, "billsec": talk, "type": kind,
            "public_external": call.other_number, "public_internal": our_number,
            "recorded": recorded, "is_voicemail": call.status == CallStatus::Voicemail,
            "fax_email": "", "is_redirected": "0", "redirected_from": "", "transferred_from": "",
            "is_local": false,
            "user_id": call.user_id.map(|user| user.to_string()).unwrap_or_default(),
            "talking_time": talk, "started_at": iso(call.started_at),
            "answered_at": call.answered_at.map(iso).unwrap_or_default(),
            "ended_at": call.ended_at.map(iso).unwrap_or_default(),
            "waiting_time": "0", "wrapup_time": "0",
            "recording_link": if recorded { id.clone() } else { String::new() },
        },
        "Contact": {
            "id": call.customer_id.unwrap_or(0),
            "name": call.customer_name.as_deref().map(str::trim).unwrap_or_default(),
            "title": "", "company": "", "industry": "", "address": "", "city": "", "zip": "",
            "state": "", "type": "", "contact_numbers": [call.other_number],
            "contact_emails": [], "tags": [], "external_urls": [], "custom_fields": [],
            "favorite_agent": empty_agent(),
        },
        "CallNumber": {
            "id": call.number_id.unwrap_or(0), "internal_name": label,
            "caller_id_e164": our_number, "country_code": 0, "area_code": 0,
        },
        "BillingCall": { "price": "0" },
        "Agent": agent,
        "Notes": [], "Tags": [], "Ratings": [],
        "Telnyx": {
            "call_id": call.id, "status": call.status, "line_label": call.line_label,
            "our_number": call.our_number, "customer_id": call.customer_id,
            "deal_id": call.deal_id, "transcript_status": call.transcript_status,
            "ai_note": ai_note, "note_added": call.note_added, "via_cell": call.via_cell,
            "spam_likely": call.spam_likely, "may_hear": call.may_hear,
        },
    })
}

/// Where the company's call `id` keeps its recording in our bucket, once
/// it is copied there.
pub async fn recording_key(
    pool: &MySqlPool,
    company_id: i32,
    id: i64,
) -> sqlx::Result<Option<String>> {
    let key: Option<Option<String>> = sqlx::query_scalar(
        "SELECT CASE WHEN recording_status = 'copied' THEN recording_key END
           FROM phone_calls WHERE id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(key.flatten())
}

/// A database error while reading a call for its audio (logged; the
/// recording counts as not fetched).
fn unreadable(id: i64, err: &sqlx::Error) -> CallTranscriptError {
    tracing::warn!(call_id = id, error = %err, "call not read for its recording");
    CallTranscriptError::FetchFailed
}

/// The audio of the company's call `id` from our bucket, and its type.
pub async fn audio(
    state: &AppState,
    company_id: i32,
    id: i64,
) -> Result<(Vec<u8>, String), CallTranscriptError> {
    let key = recording_key(&state.pool, company_id, id)
        .await
        .map_err(|err| unreadable(id, &err))?
        .ok_or(CallTranscriptError::NotAvailable)?;
    let bucket = state
        .phone
        .recordings_bucket
        .as_deref()
        .ok_or(CallTranscriptError::NotAvailable)?;
    let object = state
        .storage
        .get_url(&format!("s3://{bucket}/{key}"))
        .await
        .map_err(|_| CallTranscriptError::FetchFailed)?
        .ok_or(CallTranscriptError::NotAvailable)?;
    if object.bytes.is_empty() {
        return Err(CallTranscriptError::Empty);
    }
    let content_type = if object.content_type.is_empty() {
        "audio/mpeg".to_string()
    } else {
        object.content_type
    };
    Ok((object.bytes, content_type))
}

/// The company's call `id` as text: the transcript kept on the call (the
/// AI note's, or an earlier reader's), else its recording transcribed now
/// (speaker labels on, as the AI notes are written) and kept for the AI
/// note and the next reader.
pub async fn transcript(
    state: &AppState,
    caller: Caller<'_>,
    company_id: i32,
    id: i64,
) -> Result<Transcript, CallTranscriptError> {
    let kept: Option<Option<String>> =
        sqlx::query_scalar("SELECT transcript FROM phone_calls WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(company_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|err| unreadable(id, &err))?;
    let Some(kept) = kept else {
        return Err(CallTranscriptError::NotAvailable);
    };
    if let Some(text) = kept.filter(|text| !text.trim().is_empty()) {
        return Ok(Transcript::from_labeled_text(&text));
    }
    let (audio, _) = audio(state, company_id, id).await?;
    let mut transcript = state
        .transcriber
        .transcribe(&state.pool, caller, audio, true)
        .await?;
    let text = transcript.labeled_text();
    if text.is_empty() {
        return Err(CallTranscriptError::NoSpeech);
    }
    let saved = sqlx::query(
        "UPDATE phone_calls SET transcript = ?
          WHERE id = ? AND company_id = ? AND (transcript IS NULL OR TRIM(transcript) = '')",
    )
    .bind(&text)
    .bind(id)
    .bind(company_id)
    .execute(&state.pool)
    .await;
    if let Err(err) = saved {
        tracing::warn!(call_id = id, error = %err, "transcript not kept on the call");
    }
    transcript.text = text;
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crm_ids_name_our_calls_strictly() {
        assert_eq!(token(981_001), "tx_981001");
        assert_eq!(parse_token(" tx_981001 "), Some(981_001));
        for bad in [
            "981001",
            "tx_",
            "tx_0",
            "tx_-1",
            "tx_12a",
            "rec_5",
            "tx_1234567890123456789",
        ] {
            assert_eq!(parse_token(bad), None, "{bad}");
        }
    }
}
