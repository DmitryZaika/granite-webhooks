//! `POST /v1/jobs/call-points`: reps' outbound calls into call points.
//!
//! Pulls outbound `CloudTalk` and `RingCentral` calls into
//! `phone_call_points`, then classifies a few calls long enough to need a
//! transcript (attempt, voicemail or conversation).
//!
//! Port of `callPointsSync.server.ts` and `callPoints.ts`. The Remix job
//! classified in the background after answering; here each run classifies
//! up to [`CLASSIFY_PER_RUN`] calls before it answers, and the next run
//! takes the rest.

use super::JobCaller;
use crate::ai::Caller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::routes::calls::recording::{CallTranscriptError, transcribe_call};
use crate::state::AppState;
use crate::voicemail::classify;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use common::telephony::{cloudtalk, ringcentral};
use lambda_http::tracing;
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

const CLOUDTALK_PAGE: usize = 1000;
const MAX_PAGES: usize = 10;
/// Re-read a day (and slack) before the newest stored call, so a call still
/// ringing last run is picked up once it ends.
const LOOKBACK_HOURS: i64 = 26;
/// The CRM's `CALL_ATTEMPT_MAX_TALK_SECONDS`.
const ATTEMPT_MAX_TALK_SECONDS: i64 = 30;
const CLASSIFY_MIN_AGE_MINUTES: i64 = 10;
const CLAIM_TTL_MINUTES: i64 = 15;
const MAX_TRANSCRIBE_ATTEMPTS: u8 = 5;
/// Calls transcribed per run, three at a time.
pub const CLASSIFY_PER_RUN: i64 = 6;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct CallPointsRun {
    /// Calls written or refreshed.
    pub processed: usize,
    /// Calls classified from a transcript.
    pub classified: usize,
}

/// A provider call in the shape both providers map to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundCall {
    pub call_id: String,
    /// The provider's agent id (`CloudTalk` user, `RingCentral` extension).
    pub agent: String,
    pub customer_digits: String,
    pub started_at: DateTime<Utc>,
    pub talk_seconds: i64,
    pub recording_link: Option<String>,
    pub provider_voicemail: bool,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn last10(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    digits[digits.len().saturating_sub(10)..].to_string()
}

/// The CRM's `mapCloudTalkCall`.
pub fn cloudtalk_call(cdr: &Value) -> Option<OutboundCall> {
    if cdr["type"] != "outgoing" {
        return None;
    }
    let customer_digits = last10(cdr["public_external"].as_str().unwrap_or(""));
    if customer_digits.len() != 10 {
        return None;
    }
    let started =
        NaiveDateTime::parse_from_str(cdr["started_at"].as_str()?, "%Y-%m-%d %H:%M:%S").ok()?;
    Some(OutboundCall {
        call_id: text(&cdr["id"])?,
        agent: text(&cdr["user_id"])?,
        customer_digits,
        started_at: started.and_utc(),
        talk_seconds: text(&cdr["talking_time"])
            .and_then(|t| t.parse::<f64>().ok())
            .map_or(0, |t| {
                #[allow(clippy::cast_possible_truncation)]
                let seconds = t as i64;
                seconds
            }),
        recording_link: text(&cdr["recording_link"]),
        provider_voicemail: cdr["is_voicemail"] == true,
    })
}

/// The CRM's `mapRingCentralCall`: only a connected call has talk time.
pub fn ringcentral_call(record: &Value) -> Option<OutboundCall> {
    if !record["direction"]
        .as_str()
        .is_some_and(|d| d.eq_ignore_ascii_case("outbound"))
    {
        return None;
    }
    let agent =
        text(&record["extension"]["id"]).or_else(|| text(&record["from"]["extensionId"]))?;
    let customer_digits = last10(record["to"]["phoneNumber"].as_str().unwrap_or(""));
    if customer_digits.len() != 10 {
        return None;
    }
    let result = record["result"].as_str().unwrap_or("").to_lowercase();
    let connected = matches!(result.as_str(), "call connected" | "accepted" | "voicemail");
    Some(OutboundCall {
        call_id: text(&record["id"])?,
        agent,
        customer_digits,
        started_at: record["startTime"].as_str()?.parse().ok()?,
        talk_seconds: if connected {
            record["duration"].as_i64().unwrap_or(0)
        } else {
            0
        },
        recording_link: text(&record["recording"]["id"]).map(|id| format!("rec_{id}")),
        provider_voicemail: result == "voicemail",
    })
}

async fn cloudtalk_calls(
    state: &AppState,
    company_id: i32,
    since: DateTime<Utc>,
) -> Result<Vec<OutboundCall>, cloudtalk::ApiError> {
    let api = cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    };
    let from = percent_encoding::utf8_percent_encode(
        &since.format("%Y-%m-%d %H:%M:%S").to_string(),
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string();
    let mut calls = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = cloudtalk::request(
            api,
            Method::GET,
            &format!(
                "calls/index.json?date_from={from}&type=outgoing&limit={CLOUDTALK_PAGE}&page={page}"
            ),
            None,
            None,
        )
        .await?;
        let data = body["responseData"]["data"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        calls.extend(data.iter().filter_map(|item| cloudtalk_call(&item["Cdr"])));
        if data.len() < CLOUDTALK_PAGE {
            break;
        }
    }
    Ok(calls)
}

async fn ringcentral_calls(
    state: &AppState,
    company_id: i32,
    since: DateTime<Utc>,
) -> Result<Vec<OutboundCall>, ringcentral::ApiError> {
    let from = percent_encoding::utf8_percent_encode(
        &since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        percent_encoding::NON_ALPHANUMERIC,
    )
    .to_string();
    let mut path =
        format!("account/~/call-log?direction=Outbound&view=Detailed&dateFrom={from}&perPage=250");
    let mut calls = Vec::new();
    for _ in 0..MAX_PAGES {
        let body = ringcentral::request(
            &state.pool,
            &state.telephony_http,
            company_id,
            Method::GET,
            &path,
            &ringcentral::Body::Empty,
            None,
        )
        .await?;
        calls.extend(
            body["records"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(ringcentral_call),
        );
        // The next page is a full URI; keep what follows the API root.
        let Some(next) = body["navigation"]["nextPage"]["uri"]
            .as_str()
            .and_then(|uri| {
                uri.split_once("/restapi/v1.0/")
                    .map(|(_, rest)| rest.to_string())
            })
        else {
            break;
        };
        path = next;
    }
    Ok(calls)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    CloudTalk,
    RingCentral,
}

impl Provider {
    const fn name(self) -> &'static str {
        match self {
            Self::CloudTalk => "cloudtalk",
            Self::RingCentral => "ringcentral",
        }
    }

    const fn agent_column(self) -> &'static str {
        match self {
            Self::CloudTalk => "cloudtalk_agent_id",
            Self::RingCentral => "ringcentral_extension_id",
        }
    }
}

async fn sync_company(
    state: &AppState,
    company_id: i32,
    provider: Provider,
    counts_from: DateTime<Utc>,
) -> Result<usize, String> {
    let column = provider.agent_column();
    let agents: Vec<(i32, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, TRIM({column}) FROM users
          WHERE company_id = ? AND is_deleted = 0 AND TRIM(COALESCE({column}, '')) <> ''"
    )))
    .bind(company_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|err| err.to_string())?;
    if agents.is_empty() {
        return Ok(0);
    }
    let agents: HashMap<String, i32> = agents.into_iter().map(|(id, agent)| (agent, id)).collect();
    let newest: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT MAX(started_at) FROM phone_call_points WHERE company_id = ? AND provider = ?",
    )
    .bind(company_id)
    .bind(provider.name())
    .fetch_one(&state.pool)
    .await
    .map_err(|err| err.to_string())?;
    let since = newest
        .map(|newest| newest.and_utc() - Duration::hours(LOOKBACK_HOURS))
        .filter(|since| *since > counts_from)
        .unwrap_or(counts_from);
    let calls = match provider {
        Provider::CloudTalk => cloudtalk_calls(state, company_id, since)
            .await
            .map_err(|err| format!("cloudtalk {}", err.status))?,
        Provider::RingCentral => ringcentral_calls(state, company_id, since)
            .await
            .map_err(|err| format!("ringcentral {}", err.status))?,
    };
    let rows: Vec<(OutboundCall, i32)> = calls
        .into_iter()
        .filter(|call| call.started_at >= counts_from)
        .filter_map(|call| agents.get(&call.agent).map(|user| (call, *user)))
        .collect();
    for chunk in rows.chunks(500) {
        let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO phone_call_points (company_id, provider, provider_call_id, user_id, customer_digits,
                started_at, talk_seconds, outcome, recording_link, provider_voicemail, classified_at) ",
        );
        builder.push_values(chunk, |mut row, (call, user_id)| {
            let attempt = call.talk_seconds <= ATTEMPT_MAX_TALK_SECONDS;
            row.push_bind(company_id)
                .push_bind(provider.name())
                .push_bind(call.call_id.clone())
                .push_bind(*user_id)
                .push_bind(call.customer_digits.clone())
                .push_bind(call.started_at.naive_utc())
                .push_bind(call.talk_seconds)
                .push_bind(if attempt { "attempt" } else { "pending" })
                .push_bind(call.recording_link.clone())
                .push_bind(call.provider_voicemail)
                .push_bind(attempt.then(|| Utc::now().naive_utc()));
        });
        // classified_at is assigned before outcome: both read the old outcome.
        builder.push(
            " AS new ON DUPLICATE KEY UPDATE
                user_id = new.user_id, started_at = new.started_at, talk_seconds = new.talk_seconds,
                customer_digits = new.customer_digits, recording_link = new.recording_link,
                provider_voicemail = new.provider_voicemail,
                classified_at = IF(phone_call_points.outcome = 'pending', new.classified_at, phone_call_points.classified_at),
                outcome = IF(phone_call_points.outcome = 'pending', new.outcome, phone_call_points.outcome)",
        );
        builder
            .build()
            .execute(&state.pool)
            .await
            .map_err(|err| err.to_string())?;
    }
    Ok(rows.len())
}

/// Pulls every linked company's outbound calls since the points start date.
pub async fn sync(state: &AppState) -> Result<usize, sqlx::Error> {
    let counts_from: Option<NaiveDateTime> = sqlx::query_scalar(
        "SELECT counts_from FROM activity_points_sources WHERE source = 'calls'",
    )
    .fetch_optional(&state.pool)
    .await?;
    let counts_from = counts_from.map_or(DateTime::UNIX_EPOCH, |from| from.and_utc());
    let mut processed = 0;
    for provider in [Provider::CloudTalk, Provider::RingCentral] {
        let column = provider.agent_column();
        let companies: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT company_id FROM users WHERE is_deleted = 0 AND TRIM(COALESCE({column}, '')) <> ''"
        )))
        .fetch_all(&state.pool)
        .await?;
        for company_id in companies {
            match sync_company(state, company_id, provider, counts_from).await {
                Ok(count) => processed += count,
                // Not connected (401) or rate limited: the next run tries again.
                Err(err) => {
                    tracing::info!(company_id, provider = provider.name(), error = %err, "call points sync skipped");
                }
            }
        }
    }
    Ok(processed)
}

#[derive(sqlx::FromRow)]
struct Pending {
    id: i64,
    company_id: i32,
    provider_call_id: String,
    recording_link: Option<String>,
    provider_voicemail: bool,
    transcribe_attempts: u8,
}

/// SQL: the rows this job classifies. Granite Phone's own calls
/// (`provider = 'telnyx'`) are scored by the phone job from their AI note's
/// transcript, never here.
const PROVIDERS: &str = "provider IN ('cloudtalk', 'ringcentral')";

async fn finalize(state: &AppState, id: i64, outcome: &str) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_call_points SET outcome = ?, classified_at = UTC_TIMESTAMP()
          WHERE id = ? AND outcome = 'pending' AND {PROVIDERS}"
    )))
    .bind(outcome)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn classify_one(state: &AppState, row: Pending) -> Result<bool, sqlx::Error> {
    let claimed = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE phone_call_points SET transcribe_claimed_at = UTC_TIMESTAMP(), transcribe_attempts = transcribe_attempts + 1
          WHERE id = ? AND outcome = 'pending' AND {PROVIDERS}
            AND (transcribe_claimed_at IS NULL OR transcribe_claimed_at < UTC_TIMESTAMP() - INTERVAL {CLAIM_TTL_MINUTES} MINUTE)"
    )))
    .bind(row.id)
    .execute(&state.pool)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Ok(false);
    }
    // No recording proves nothing: it counts as an attempt.
    let Some(link) = row.recording_link.as_deref() else {
        finalize(state, row.id, "attempt").await?;
        return Ok(true);
    };
    let caller = Caller {
        feature: "call_points",
        user_id: None,
        company_id: Some(row.company_id),
    };
    match transcribe_call(state, caller, &row.provider_call_id, Some(link), false).await {
        Ok(transcript) => {
            finalize(
                state,
                row.id,
                classify(row.provider_voicemail, &transcript.text),
            )
            .await?;
            Ok(true)
        }
        Err(err) => {
            let permanent = matches!(
                err,
                CallTranscriptError::Empty | CallTranscriptError::NoSpeech
            );
            tracing::warn!(row_id = row.id, error = %err, "call points transcription failed");
            if permanent || row.transcribe_attempts + 1 >= MAX_TRANSCRIBE_ATTEMPTS {
                finalize(state, row.id, "attempt").await?;
                return Ok(true);
            }
            // Left pending: the claim lapses and a later run retries.
            Ok(false)
        }
    }
}

/// Transcribes and classifies up to `limit` pending calls, three at a time.
pub async fn classify_pending(state: &AppState, limit: i64) -> Result<usize, sqlx::Error> {
    let rows: Vec<Pending> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, company_id, provider_call_id, recording_link, provider_voicemail <> 0 AS provider_voicemail,
                transcribe_attempts
           FROM phone_call_points
          WHERE outcome = 'pending' AND {PROVIDERS} AND user_id IS NOT NULL AND started_at IS NOT NULL
            AND started_at <= UTC_TIMESTAMP() - INTERVAL {CLASSIFY_MIN_AGE_MINUTES} MINUTE
            AND (transcribe_claimed_at IS NULL OR transcribe_claimed_at < UTC_TIMESTAMP() - INTERVAL {CLAIM_TTL_MINUTES} MINUTE)
          ORDER BY started_at LIMIT ?"
    )))
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    let mut classified = 0;
    let mut rows = rows.into_iter();
    loop {
        let batch: Vec<Pending> = rows.by_ref().take(3).collect();
        if batch.is_empty() {
            return Ok(classified);
        }
        let tasks: Vec<_> = batch
            .into_iter()
            .map(|row| {
                let state = state.clone();
                tokio::spawn(async move { classify_one(&state, row).await })
            })
            .collect();
        for task in tasks {
            match task.await {
                Ok(Ok(true)) => classified += 1,
                Ok(Ok(false)) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) => tracing::error!(error = %err, "call points task panicked"),
            }
        }
    }
}

/// Sync and classify call points
///
/// Pulls reps' outbound calls into the call points table, then transcribes
/// and classifies a few of the calls long enough to need it.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/call-points",
    operation_id = "run_call_points",
    tag = "jobs",
    responses(
        (status = 200, description = "What was done", body = CallPointsRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<CallPointsRun>> {
    let processed = sync(&state).await?;
    let classified = classify_pending(&state, CLASSIFY_PER_RUN).await?;
    Ok(Json(CallPointsRun {
        processed,
        classified,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_calls_map_like_the_crm() {
        let call = cloudtalk_call(&json!({
            "id": "77", "type": "outgoing", "user_id": " 5101 ", "public_external": "+1 (317) 555-0100",
            "started_at": "2026-05-01 10:00:00", "talking_time": "45", "recording_link": "https://x/r.wav",
        }))
        .unwrap();
        assert_eq!(
            (
                call.agent.as_str(),
                call.customer_digits.as_str(),
                call.talk_seconds
            ),
            ("5101", "3175550100", 45)
        );
        assert!(cloudtalk_call(&json!({ "id": "1", "type": "incoming" })).is_none());
        let rc = ringcentral_call(&json!({
            "id": "a1", "direction": "Outbound", "extension": { "id": 42 }, "to": { "phoneNumber": "+13175550100" },
            "startTime": "2026-05-01T10:00:00.000Z", "result": "No Answer", "duration": 25, "recording": { "id": "9" },
        }))
        .unwrap();
        assert_eq!((rc.agent.as_str(), rc.talk_seconds), ("42", 0));
        assert_eq!(rc.recording_link.as_deref(), Some("rec_9"));
    }
}
