//! Calls through the company's telephony provider.
//!
//! - `GET /v1/calls/{provider}` → a deal's or customer's calls, in the
//!   `CloudTalk` call shape (`RingCentral` rows are converted)
//! - `POST /v1/calls/{provider}` → call a number: `CloudTalk` rings the
//!   caller's agent first, `RingCentral` rings their line (`RingOut`)
//! - `POST /v1/call-transcripts` → a recorded call's transcript
//!
//! Replaces the Remix `api.{cloudtalk,ringcentral}.{dealCalls,customerCalls}`,
//! `api.cloudtalk.call` and `api.ringcentral.ringout` routes.

pub mod recording;
pub mod ringcentral;
mod transcript;

use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::company::queries::company_features;
use crate::routes::sms::provider::SmsProvider;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use common::telephony::cloudtalk;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write;
use std::time::{Duration, Instant};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, start))
        .routes(routes!(transcript::transcribe))
}

/// Query of `GET /v1/calls/{provider}`: a deal, or one or two phones.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallsQuery {
    /// Calls with the deal's customer since the deal was created.
    pub deal_id: Option<i64>,
    /// Or: calls with this phone.
    pub phone: Option<String>,
    /// And this one.
    pub phone2: Option<String>,
    /// From this day (`YYYY-MM-DD`), with `phone`.
    pub date_from: Option<String>,
    /// To this day, with `phone`.
    pub date_to: Option<String>,
}

/// `GET /v1/calls/{provider}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Calls {
    /// Calls in the `CloudTalk` shape (`Cdr`, `Contact`, `Agent`, …).
    pub items: Vec<Value>,
}

/// `+1XXXXXXXXXX` of a US phone, or a `+` number as typed.
pub fn to_e164(phone: &str) -> Option<String> {
    let trimmed = phone.trim();
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if trimmed.starts_with('+') {
        return (!digits.is_empty()).then(|| format!("+{digits}"));
    }
    match digits.len() {
        10 => Some(format!("+1{digits}")),
        11 if digits.starts_with('1') => Some(format!("+{digits}")),
        _ => None,
    }
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn id_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

/// `CloudTalk` calls with any of `phones`, each call once.
async fn cloudtalk_calls(
    state: &AppState,
    company_id: i32,
    phones: &[String],
    date_from: Option<&str>,
    date_to: Option<&str>,
) -> Result<Vec<Value>, cloudtalk::ApiError> {
    let api = cloudtalk::Api {
        pool: &state.pool,
        client: &state.telephony_http,
        base_url: &state.cloudtalk_base,
        company_id,
    };
    let encode = |value: &str| {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    };
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for phone in phones {
        let mut path = format!(
            "calls/index.json?limit=200&public_external={}",
            encode(phone)
        );
        for (key, value) in [("date_from", date_from), ("date_to", date_to)] {
            if let Some(value) = value {
                let _ = write!(path, "&{key}={}", encode(value));
            }
        }
        let body = cloudtalk::request(api, Method::GET, &path, None, None).await?;
        for item in body["responseData"]["data"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if seen.insert(id_text(&item["Cdr"]["id"])) {
                items.push(item.clone());
            }
        }
    }
    Ok(items)
}

#[derive(sqlx::FromRow)]
struct DealPhones {
    phone: Option<String>,
    phone_2: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

/// List calls with a customer
///
/// With `deal_id`: calls with the deal's customer since the deal was created
/// (none for another company's deal). With `phone`/`phone2`: every call with
/// those numbers in the date range. Empty when the company has not set up
/// the provider.
#[utoipa::path(
    get,
    path = "/v1/calls/{provider}",
    operation_id = "list_calls",
    tag = "calls",
    params(("provider" = SmsProvider, Path, description = "Telephony provider"), CallsQuery),
    responses(
        (status = 200, description = "Calls", body = Calls),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 429, description = "RingCentral is rate limiting; try again in a minute", body = ErrorBody),
        (status = 502, description = "The provider failed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    Query(query): Query<CallsQuery>,
) -> ApiResult<Response> {
    let (phones, date_from) = match query.deal_id {
        Some(deal_id) => {
            let deal: Option<DealPhones> = sqlx::query_as(
                "SELECT c.phone, c.phone_2, d.created_at FROM deals d JOIN customers c ON c.id = d.customer_id
                  WHERE d.id = ? AND c.company_id = ? AND d.deleted_at IS NULL",
            )
            .bind(deal_id)
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
            let Some(deal) = deal else {
                return Ok(Json(Calls { items: Vec::new() }).into_response());
            };
            (
                vec![deal.phone, deal.phone_2],
                deal.created_at.map(|at| at.format("%Y-%m-%d").to_string()),
            )
        }
        None => (vec![query.phone, query.phone2], query.date_from),
    };
    let phones: Vec<String> = phones
        .iter()
        .flatten()
        .filter_map(|phone| to_e164(phone))
        .collect();
    let date_to = query.deal_id.is_none().then_some(query.date_to).flatten();
    let features = company_features(&state.pool, user.company_id).await?;
    let items = match provider {
        SmsProvider::Cloudtalk if features.cloudtalk => {
            match cloudtalk_calls(
                &state,
                user.company_id,
                &phones,
                date_from.as_deref(),
                date_to.as_deref(),
            )
            .await
            {
                Ok(items) => items,
                Err(err) => {
                    return Ok(refuse(
                        StatusCode::BAD_GATEWAY,
                        &format!("cloudtalk_{}", err.status),
                    ));
                }
            }
        }
        SmsProvider::Ringcentral if features.ringcentral => {
            let fetch = ringcentral::Fetch {
                pool: &state.pool,
                client: &state.telephony_http,
                company_id: user.company_id,
            };
            match fetch
                .calls_for_phones(&phones, date_from.as_deref(), date_to.as_deref())
                .await
            {
                Ok(items) => items,
                Err(err) if err.status == 429 => {
                    return Ok(refuse(
                        StatusCode::TOO_MANY_REQUESTS,
                        "RingCentral is busy, try again in a minute",
                    ));
                }
                Err(err) => {
                    return Ok(refuse(
                        StatusCode::BAD_GATEWAY,
                        &format!("ringcentral_{}", err.status),
                    ));
                }
            }
        }
        _ => Vec::new(),
    };
    Ok(Json(Calls { items }).into_response())
}

/// Body of `POST /v1/calls/{provider}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CallInput {
    /// 10 to 15 digits.
    pub phone_digits: String,
}

/// `POST /v1/calls/{provider}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct StartedCall {
    /// `RingCentral`'s call status, e.g. `InProgress`.
    pub call_status: Option<String>,
    /// The caller's own line `RingCentral` rings first.
    pub ringing_line: Option<String>,
}

/// One of the caller's 10 (or `CALL_RATE_LIMIT_PER_MIN`) calls a minute.
fn take_call_slot(state: &AppState, user_id: i32) -> bool {
    let limit = std::env::var("CALL_RATE_LIMIT_PER_MIN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(10);
    let mut slots = state
        .call_starts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let window = Duration::from_secs(60);
    let starts: &mut VecDeque<Instant> = slots.entry(user_id).or_default();
    starts.retain(|at| now.duration_since(*at) < window);
    let free = starts.len() < limit;
    if free {
        starts.push_back(now);
    }
    drop(slots);
    free
}

fn cloudtalk_error(err: &cloudtalk::ApiError) -> String {
    let message = err.message.as_deref().unwrap_or("").to_lowercase();
    if err.status == 403 || message.contains("not online") {
        "agent_not_online".into()
    } else if message.contains("invalid") || message.contains("phone") {
        "invalid_phone".into()
    } else {
        format!("cloudtalk_{}", err.status)
    }
}

fn ringcentral_error(err: &common::telephony::ringcentral::ApiError) -> String {
    let message = err.message.as_deref().unwrap_or("").to_lowercase();
    if message.contains("ringout") || message.contains("permission") {
        "ringcentral_no_call_permission".into()
    } else if message.contains("invalid") || message.contains("phone number") {
        "ringcentral_invalid_phone".into()
    } else {
        format!("ringcentral_{}", err.status)
    }
}

/// Call a number
///
/// `CloudTalk` rings the caller's agent (`CloudTalk` app or phone), then dials
/// the number once they answer. `RingCentral` rings the caller's own line,
/// then connects the number. Errors are tokens the page explains:
/// `cloudtalk_not_configured`, `agent_not_linked`, `agent_not_online`,
/// `ringcentral_not_configured`, `my_line_missing`,
/// `ringcentral_no_call_permission`, `invalid_phone`, `rate_limited`.
#[utoipa::path(
    post,
    path = "/v1/calls/{provider}",
    operation_id = "start_call",
    tag = "calls",
    params(("provider" = SmsProvider, Path, description = "Telephony provider")),
    request_body = CallInput,
    responses(
        (status = 200, description = "Ringing", body = StartedCall),
        (status = 400, description = "Not a phone number", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Provider not set up, or the caller has no agent or line", body = ErrorBody),
        (status = 429, description = "More than 10 calls a minute", body = ErrorBody),
        (status = 502, description = "The provider refused", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    JsonBody(input): JsonBody<CallInput>,
) -> ApiResult<Response> {
    let features = company_features(&state.pool, user.company_id).await?;
    let digits = input.phone_digits.trim();
    let valid = (10..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    match provider {
        SmsProvider::Cloudtalk => {
            if !features.cloudtalk {
                return Ok(refuse(StatusCode::FORBIDDEN, "cloudtalk_not_configured"));
            }
            let agent = user
                .cloudtalk_agent_id
                .as_deref()
                .map(str::trim)
                .and_then(|id| id.parse::<i64>().ok())
                .filter(|id| *id > 0);
            let Some(agent) = agent else {
                return Ok(refuse(StatusCode::FORBIDDEN, "agent_not_linked"));
            };
            let Some(to) = to_e164(digits).filter(|_| valid) else {
                return Ok(refuse(StatusCode::BAD_REQUEST, "invalid_phone"));
            };
            if !take_call_slot(&state, user.id) {
                return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
            }
            let api = cloudtalk::Api {
                pool: &state.pool,
                client: &state.telephony_http,
                base_url: &state.cloudtalk_base,
                company_id: user.company_id,
            };
            let body = json!({ "agent_id": agent, "callee_number": to });
            match cloudtalk::request(api, Method::POST, "calls/create.json", Some(&body), None)
                .await
            {
                Ok(_) => Ok(Json(StartedCall {
                    call_status: None,
                    ringing_line: None,
                })
                .into_response()),
                Err(err) => Ok(refuse(StatusCode::BAD_GATEWAY, &cloudtalk_error(&err))),
            }
        }
        SmsProvider::Ringcentral => {
            if !features.ringcentral {
                return Ok(refuse(StatusCode::FORBIDDEN, "ringcentral_not_configured"));
            }
            let Some(line) = user.ringcentral_phone_number.as_deref().and_then(to_e164) else {
                return Ok(refuse(StatusCode::FORBIDDEN, "my_line_missing"));
            };
            let Some(to) = to_e164(digits).filter(|_| valid) else {
                return Ok(refuse(StatusCode::BAD_REQUEST, "invalid_phone"));
            };
            if !take_call_slot(&state, user.id) {
                return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
            }
            let body = json!({
                "from": { "phoneNumber": line }, "to": { "phoneNumber": to }, "playPrompt": false,
            });
            let result = common::telephony::ringcentral::request(
                &state.pool,
                &state.telephony_http,
                user.company_id,
                Method::POST,
                "account/~/extension/~/ring-out",
                &common::telephony::ringcentral::Body::Json(&body),
                None,
            )
            .await;
            match result {
                Ok(reply) => Ok(Json(StartedCall {
                    call_status: reply["status"]["callStatus"].as_str().map(str::to_string),
                    ringing_line: Some(line),
                })
                .into_response()),
                Err(err) => Ok(refuse(StatusCode::BAD_GATEWAY, &ringcentral_error(&err))),
            }
        }
    }
}
