//! The dashboard's call coach.
//!
//! - `GET /v1/me/call-coaching` → the rep's last three real calls (incoming
//!   or outgoing, over a minute of talk, last 30 days) with their scores
//! - `POST /v1/me/call-coaching` → transcribe and score one of them
//!
//! Port of `callCoaching.server.ts` (prompt unchanged). The Remix server kept
//! scores in memory and scored in the background; here `POST` scores inside
//! the request and stores the result in `call_coaching_analyses`, so a page
//! that gave up waiting finds it on its next poll.

use crate::ai::{Caller, Chat, MINI_MODEL};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::calls::recording::transcribe_call;
use crate::routes::events::notify::display_phone;
use crate::routes::users::call_activity::{
    agent_calls, company_ringcentral_calls, ringcentral_item,
};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

const LOOKBACK_DAYS: i64 = 30;
const DISPLAY_CALLS: usize = 3;
/// The CRM's `CALL_COACHING_MIN_TALKING_SECONDS`.
const MIN_TALKING_SECONDS: f64 = 60.0;
/// A score still pending after this long died with its request.
const PENDING_TIMEOUT_MINUTES: i64 = 10;

const SYSTEM_PROMPT: &str = r#"You coach countertop and stone sales reps on phone calls. The user asked you to score THIS call. Always score it. Never skip. Do not treat already-bought, install-status, quote, showroom, voicemail, or "not sales" as a reason to refuse a score.

Score first calls, follow-ups, existing-order calls, install/measurement questions, and any other live conversation the same way: how well the rep handled THIS call.

When scoring, score 1-100 on how well the rep advanced THIS call's goal. Judge the conversation, not the outcome. A customer who does not book or buy can still be a high score if the rep used the right approach. A booked visit or verbal yes can still be a low score if the rep pitched, pressured, or sounded unsure.

Score the rep on:
- Questions first, not a pitch. They should learn the customer's situation, what is not working, what they want instead, and (on a follow-up) what is holding the decision.
- Calm, curious, confident tone. Not needy, not "just checking in", not dumping features, brands, or square-foot pricing before they understand the job.
- A clear next step earned with questions: first call, invite to the showroom with a choice of times ("Thursday morning or Saturday afternoon"); follow-up, a decision, a return visit, or a concrete close — not a vague "let me know".
- Finding who else decides, timeline, and the real concern under "I need to think about it" instead of arguing.
- Not talking over the customer. Not answering a question they were not asked with a speech.

Bands:
- 67-100: strong questions, confident tone, next step tied to what the customer said.
- 34-66: some good questions, but a pitch, a weak next step, or missed decision-maker / concern.
- 1-33: mostly telling, pressuring, uncertain, or no real attempt to book, follow up, or close.

advice: 2-4 sentences, specific to this transcript. Quote or paraphrase what the rep actually said when it helps. Tell them what to do differently next time. Never name a book, author, course, or branded sales system.
summary: one short sentence of what the call was about.
Return JSON only with keys: relevant, skip_reason, score, advice, summary.
Always set relevant=true and skip_reason=null. Always return a score from 1-100 and non-empty advice."#;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list, evaluate))
}

/// Where a call's score stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CoachingStatus {
    Idle,
    Pending,
    Ready,
    Error,
}

/// A call the coach can score.
#[derive(Debug, Serialize, ToSchema)]
pub struct CoachingCall {
    /// `CloudTalk` or `RingCentral` call id.
    pub call_id: String,
    pub status: CoachingStatus,
    /// When it started (`CloudTalk` time, or RFC 3339 for `RingCentral`).
    pub started_at: String,
    /// Seconds talked.
    pub talking_time: f64,
    /// `incoming` or `outgoing`.
    pub call_type: String,
    /// The customer, from the CRM or the phone system.
    pub customer_name: Option<String>,
    /// The other party's number, dashed.
    pub phone_display: String,
    /// 1–100 once scored.
    pub score: Option<u8>,
    /// What to do differently.
    pub advice: Option<String>,
    /// What the call was about.
    pub summary: Option<String>,
    /// Why it could not be scored.
    pub error_message: Option<String>,
}

/// `GET`/`POST /v1/me/call-coaching`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Coaching {
    /// The caller has a `CloudTalk` line (agent and number) or a
    /// `RingCentral` line (extension and number).
    pub eligible: bool,
    /// Some call is being scored.
    pub analyzing: bool,
    /// Newest first.
    pub calls: Vec<CoachingCall>,
}

struct Candidate {
    call_id: String,
    started_at: String,
    talking_time: f64,
    call_type: String,
    recorded: bool,
    recording_link: Option<String>,
    phone: String,
    contact_name: Option<String>,
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

fn number(value: &Value) -> f64 {
    text(value).parse().unwrap_or(f64::NAN)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        _ => false,
    }
}

/// The CRM's `isCallCoachingCandidate` + `toCandidate`.
fn candidate(item: &Value) -> Option<Candidate> {
    let cdr = &item["Cdr"];
    let call_type = cdr["type"].as_str()?;
    let talking_time = number(&cdr["talking_time"]);
    if !matches!(call_type, "incoming" | "outgoing")
        || truthy(&cdr["is_voicemail"])
        || !talking_time.is_finite()
        || talking_time <= MIN_TALKING_SECONDS
    {
        return None;
    }
    Some(Candidate {
        call_id: Some(text(&cdr["id"])).filter(|id| !id.is_empty())?,
        started_at: text(&cdr["started_at"]),
        talking_time,
        call_type: call_type.to_string(),
        recorded: truthy(&cdr["recorded"]),
        recording_link: Some(text(&cdr["recording_link"])).filter(|link| !link.is_empty()),
        phone: text(&cdr["public_external"]),
        contact_name: item["Contact"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
    })
}

fn present(value: Option<&String>) -> Option<&str> {
    value.map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn has_cloudtalk_line(user: &SessionUser) -> bool {
    present(user.cloudtalk_agent_id.as_ref()).is_some()
        && present(user.cloudtalk_phone_number.as_ref()).is_some()
}

fn has_ringcentral_line(user: &SessionUser) -> bool {
    present(user.ringcentral_extension_id.as_ref()).is_some()
        && present(user.ringcentral_phone_number.as_ref()).is_some()
}

fn has_line(user: &SessionUser) -> bool {
    has_cloudtalk_line(user) || has_ringcentral_line(user)
}

/// `CloudTalk`'s `2026-05-01 10:00:00` (UTC) or `RingCentral`'s RFC 3339.
fn started(at: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S")
        .map(|at| at.and_utc())
        .or_else(|_| DateTime::parse_from_rfc3339(at).map(|at| at.with_timezone(&Utc)))
        .ok()
}

async fn candidates(state: &AppState, user: &SessionUser) -> Vec<Candidate> {
    let since = Utc::now() - Duration::days(LOOKBACK_DAYS);
    let cloudtalk = async {
        if !has_cloudtalk_line(user) {
            return Vec::new();
        }
        let agent = present(user.cloudtalk_agent_id.as_ref()).unwrap_or_default();
        let from = since.format("%Y-%m-%d").to_string();
        agent_calls(state, user.company_id, agent, &from).await
    };
    let ringcentral = async {
        if !has_ringcentral_line(user) {
            return Vec::new();
        }
        let extension = present(user.ringcentral_extension_id.as_ref());
        let from = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        company_ringcentral_calls(state, user.company_id, extension, &from)
            .await
            .iter()
            .map(ringcentral_item)
            .collect()
    };
    let (cloudtalk, ringcentral) = tokio::join!(cloudtalk, ringcentral);
    let mut calls: Vec<Candidate> = cloudtalk
        .iter()
        .chain(&ringcentral)
        .filter_map(candidate)
        .collect();
    calls.sort_by_key(|call| std::cmp::Reverse(started(&call.started_at)));
    calls.truncate(DISPLAY_CALLS);
    calls
}

#[derive(sqlx::FromRow)]
struct Stored {
    call_id: String,
    status: String,
    score: Option<u8>,
    advice: Option<String>,
    summary: Option<String>,
    customer_name: Option<String>,
    error_message: Option<String>,
    updated_at: NaiveDateTime,
}

async fn snapshot(
    state: &AppState,
    user: &SessionUser,
    calls: Vec<Candidate>,
) -> Result<Coaching, sqlx::Error> {
    let mut stored: HashMap<String, Stored> = if calls.is_empty() {
        HashMap::new()
    } else {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT call_id, CAST(status AS CHAR) AS status, score, advice, summary, customer_name,
                    error_message, updated_at
               FROM call_coaching_analyses WHERE company_id = ",
        );
        builder.push_bind(user.company_id).push(" AND call_id IN (");
        let mut list = builder.separated(", ");
        for call in &calls {
            list.push_bind(call.call_id.clone());
        }
        builder.push(")");
        let rows: Vec<Stored> = builder.build_query_as().fetch_all(&state.pool).await?;
        rows.into_iter()
            .map(|row| (row.call_id.clone(), row))
            .collect()
    };
    let cutoff = Utc::now().naive_utc() - Duration::minutes(PENDING_TIMEOUT_MINUTES);
    let listed: Vec<CoachingCall> = calls
        .into_iter()
        .map(|call| {
            let mut out = CoachingCall {
                status: CoachingStatus::Idle,
                phone_display: display_phone(&call.phone),
                customer_name: call.contact_name.clone(),
                call_id: call.call_id.clone(),
                started_at: call.started_at,
                talking_time: call.talking_time,
                call_type: call.call_type,
                score: None,
                advice: None,
                summary: None,
                error_message: None,
            };
            if let Some(row) = stored.remove(&call.call_id) {
                match row.status.as_str() {
                    "ready" => {
                        out.status = CoachingStatus::Ready;
                        out.score = row.score;
                        out.advice = row.advice;
                        out.summary = row.summary.filter(|summary| !summary.is_empty());
                        out.customer_name = row.customer_name.or(out.customer_name);
                    }
                    "pending" if row.updated_at > cutoff => out.status = CoachingStatus::Pending,
                    "pending" => {
                        out.status = CoachingStatus::Error;
                        out.error_message = Some("Could not transcribe or score this call".into());
                    }
                    _ => {
                        out.status = CoachingStatus::Error;
                        out.error_message = row.error_message;
                    }
                }
            }
            out
        })
        .collect();
    Ok(Coaching {
        eligible: true,
        analyzing: listed
            .iter()
            .any(|call| call.status == CoachingStatus::Pending),
        calls: listed,
    })
}

const fn not_eligible() -> Coaching {
    Coaching {
        eligible: false,
        analyzing: false,
        calls: Vec::new(),
    }
}

/// Get my call coaching
///
/// The caller's last three real `CloudTalk` and `RingCentral` calls with
/// their scores, when they have either line.
#[utoipa::path(
    get,
    path = "/v1/me/call-coaching",
    operation_id = "get_my_call_coaching",
    tag = "me",
    responses(
        (status = 200, description = "Calls", body = Coaching),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Coaching>> {
    if !has_line(&user) {
        return Ok(Json(not_eligible()));
    }
    let calls = candidates(&state, &user).await;
    Ok(Json(snapshot(&state, &user, calls).await?))
}

/// Body of `POST /v1/me/call-coaching`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EvaluateInput {
    /// One of the calls `GET` listed.
    pub call_id: String,
}

struct CustomerContext {
    name: Option<String>,
    won: bool,
    open: bool,
}

async fn customer_context(
    state: &AppState,
    company_id: i32,
    phone: &str,
) -> Result<CustomerContext, sqlx::Error> {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    let last10 = &digits[digits.len().saturating_sub(10)..];
    if last10.len() < 10 {
        return Ok(CustomerContext {
            name: None,
            won: false,
            open: false,
        });
    }
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT c.name, MAX(CASE WHEN d.is_won = 1 THEN 1 ELSE 0 END),
                MAX(CASE WHEN d.is_won IS NULL AND d.id IS NOT NULL THEN 1 ELSE 0 END)
           FROM customers c
           LEFT JOIN deals d ON d.customer_id = c.id AND d.deleted_at IS NULL
          WHERE c.company_id = ? AND c.deleted_at IS NULL
            AND (RIGHT(REGEXP_REPLACE(COALESCE(c.phone, ''), '[^0-9]', ''), 10) = ?
                 OR RIGHT(REGEXP_REPLACE(COALESCE(c.phone_2, ''), '[^0-9]', ''), 10) = ?)
          GROUP BY c.id, c.name LIMIT 5",
    )
    .bind(company_id)
    .bind(last10)
    .bind(last10)
    .fetch_all(&state.pool)
    .await?;
    Ok(CustomerContext {
        name: rows
            .iter()
            .find_map(|(name, _, _)| {
                name.as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
            })
            .map(str::to_string),
        won: rows.iter().any(|(_, won, _)| *won == 1),
        open: rows.iter().any(|(_, _, open)| *open == 1),
    })
}

/// The CRM's `parseCallCoachingModelOutput`: a JSON object, or the first
/// `{...}` in the reply, with an integer score of 1–100 and advice.
pub fn parse_score(reply: &str) -> Option<(u8, String, String)> {
    let reply = reply.trim();
    let parsed: Value = serde_json::from_str(reply).ok().or_else(|| {
        let start = reply.find('{')?;
        let end = reply.rfind('}').filter(|end| *end > start)?;
        serde_json::from_str(&reply[start..=end]).ok()
    })?;
    let score = parsed["score"]
        .as_u64()
        .filter(|score| (1..=100).contains(score))?;
    let advice = parsed["advice"]
        .as_str()
        .map(str::trim)
        .filter(|advice| !advice.is_empty())?;
    Some((
        u8::try_from(score).ok()?,
        advice.to_string(),
        parsed["summary"].as_str().unwrap_or("").trim().to_string(),
    ))
}

async fn store_error(
    state: &AppState,
    company_id: i32,
    call_id: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE call_coaching_analyses SET status = 'error', error_message = ? WHERE company_id = ? AND call_id = ?")
        .bind(message)
        .bind(company_id)
        .bind(call_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn analyze(
    state: &AppState,
    user: &SessionUser,
    call: &Candidate,
) -> Result<(), sqlx::Error> {
    if !call.recorded {
        return store_error(
            state,
            user.company_id,
            &call.call_id,
            "No recording for this call",
        )
        .await;
    }
    let customer = customer_context(state, user.company_id, &call.phone).await?;
    let caller = Caller {
        feature: "call_transcription",
        user_id: Some(user.id),
        company_id: Some(user.company_id),
    };
    let transcript = match transcribe_call(
        state,
        caller,
        &call.call_id,
        call.recording_link.as_deref(),
        false,
    )
    .await
    {
        Ok(transcript) => transcript.text,
        Err(err) => {
            tracing::warn!(error = %err, "call coaching transcription failed");
            return store_error(
                state,
                user.company_id,
                &call.call_id,
                "Could not transcribe or score this call",
            )
            .await;
        }
    };
    let prompt = format!(
        "Call direction: {}\nTalk time seconds: {}\nCRM customer name: {}\nCRM has a won deal: {}\nCRM has an open deal: {}\nWon/open deal flags are context only. Always score this call. Do not skip.\n\nTranscript:\n{transcript}",
        call.call_type,
        call.talking_time,
        customer
            .name
            .as_deref()
            .or(call.contact_name.as_deref())
            .unwrap_or("unknown"),
        if customer.won { "yes" } else { "no" },
        if customer.open { "yes" } else { "no" },
    );
    let mut chat = Chat::new(MINI_MODEL)
        .message("system", SYSTEM_PROMPT)
        .message("user", &prompt);
    chat.json = true;
    let scoring = Caller {
        feature: "call_coaching",
        ..caller
    };
    let scored = state
        .ai
        .chat(&state.pool, scoring, &chat)
        .await
        .ok()
        .and_then(|reply| parse_score(&reply));
    let Some((score, advice, summary)) = scored else {
        return store_error(
            state,
            user.company_id,
            &call.call_id,
            "Could not score call",
        )
        .await;
    };
    sqlx::query(
        "UPDATE call_coaching_analyses
            SET status = 'ready', score = ?, advice = ?, summary = ?, customer_name = ?, error_message = NULL
          WHERE company_id = ? AND call_id = ?",
    )
    .bind(score)
    .bind(advice)
    .bind(summary)
    .bind(customer.name.or_else(|| call.contact_name.clone()))
    .bind(user.company_id)
    .bind(&call.call_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Ask the coach about a call
///
/// Transcribes and scores one of the calls `GET` lists (once; a failed one
/// is tried again), then returns the list. Scoring takes a while; a page
/// that stops waiting sees the result on its next `GET`.
#[utoipa::path(
    post,
    path = "/v1/me/call-coaching",
    operation_id = "evaluate_my_call",
    tag = "me",
    request_body = EvaluateInput,
    responses(
        (status = 200, description = "Calls, the asked one scored", body = Coaching),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn evaluate(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<EvaluateInput>,
) -> ApiResult<Json<Coaching>> {
    if !has_line(&user) {
        return Ok(Json(not_eligible()));
    }
    let calls = candidates(&state, &user).await;
    let asked = calls
        .iter()
        .find(|call| call.call_id == input.call_id.trim());
    if let Some(call) = asked
        && claim(&state, &user, &call.call_id).await?
    {
        analyze(&state, &user, call).await?;
    }
    Ok(Json(snapshot(&state, &user, calls).await?))
}

/// Marks the call pending unless it is scored or being scored; true when
/// this request should score it.
async fn claim(state: &AppState, user: &SessionUser, call_id: &str) -> Result<bool, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let current: Option<(String, NaiveDateTime)> = sqlx::query_as(
        "SELECT CAST(status AS CHAR), updated_at FROM call_coaching_analyses
          WHERE company_id = ? AND call_id = ? FOR UPDATE",
    )
    .bind(user.company_id)
    .bind(call_id)
    .fetch_optional(&mut *tx)
    .await?;
    let cutoff = Utc::now().naive_utc() - Duration::minutes(PENDING_TIMEOUT_MINUTES);
    let claimed = match &current {
        None => true,
        Some((status, updated_at)) => {
            status == "error" || (status == "pending" && *updated_at <= cutoff)
        }
    };
    if claimed {
        sqlx::query(
            "INSERT INTO call_coaching_analyses (company_id, call_id, user_id, status) VALUES (?, ?, ?, 'pending')
             ON DUPLICATE KEY UPDATE user_id = VALUES(user_id), status = 'pending', error_message = NULL,
                                     updated_at = UTC_TIMESTAMP()",
        )
        .bind(user.company_id)
        .bind(call_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(claimed)
}
#[cfg(test)]
mod tests {
    use super::parse_score;

    #[test]
    fn scores_are_read_from_the_reply() {
        assert_eq!(
            parse_score(
                r#"Sure: {"relevant":true,"skip_reason":null,"score":72,"advice":"Ask more.","summary":"Quartz"}"#
            ),
            Some((72, "Ask more.".into(), "Quartz".into()))
        );
        assert_eq!(parse_score(r#"{"score":0,"advice":"x"}"#), None);
        assert_eq!(parse_score(r#"{"score":50,"advice":" "}"#), None);
    }
}
