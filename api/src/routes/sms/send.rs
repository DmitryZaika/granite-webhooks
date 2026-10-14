//! Sending a text from a conversation, now or later.
//!
//! Replaces the Remix `api.{cloudtalk,ringcentral}.sms.send` actions. The
//! `webhooks` Lambda talks to the provider; a scheduled text goes into
//! `delayed_sms`, which the CRM's delayed-message job sends.

use super::provider::{SmsProvider, phone10};
use super::schemas::Message;
use super::thread::message_by_id;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::webhooks_client::{OutgoingSms, OutgoingSmsFile, SmsFailure};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use std::collections::VecDeque;
use std::time::Instant;
use utoipa::ToSchema;

/// Longest text the composer allows.
const MAX_TEXT: usize = 1600;
const MAX_ATTACHMENTS: usize = 10;
const MAX_DELAY_DAYS: i64 = 90;

const STORED_TYPES: [&str; 10] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "application/pdf",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/plain",
    "text/csv",
];

/// A file prepared for the text (`POST /v1/sms/attachments`).
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SmsFile {
    /// Its key, under `sms-attachments/<company id>/`.
    pub s3_key: String,
    /// Its type (JPEG, PNG, GIF, PDF, Office, text or CSV).
    pub content_type: String,
    /// The name it was uploaded with.
    #[serde(default)]
    pub filename: String,
    /// Pixel width of an image.
    pub width: Option<i32>,
    /// Pixel height of an image.
    pub height: Option<i32>,
}

/// Body of `POST /v1/sms/{provider}/messages`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SendSmsRequest {
    /// The customer's phone, 10 to 15 digits.
    pub phone_digits: String,
    /// The message (up to 1600 characters); may be empty with files.
    #[serde(default)]
    pub text: String,
    /// Up to ten files.
    #[serde(default)]
    pub attachments: Vec<SmsFile>,
    /// The SMS template it was written from (counts towards its ordering).
    pub template_id: Option<i32>,
    /// The customer it is for (scheduled texts only).
    pub customer_id: Option<i32>,
    /// Send later instead, within 90 days.
    pub send_at: Option<DateTime<Utc>>,
    /// A scheduled text is dropped if the customer writes first.
    #[serde(default)]
    pub cancel_if_customer_messages: bool,
}

/// What `POST /v1/sms/{provider}/messages` answers.
#[derive(Debug, Serialize, ToSchema)]
pub struct SentSms {
    /// The text as stored (the first message when it went out in parts);
    /// absent when it was scheduled.
    pub message: Option<Message>,
    /// It was scheduled for `send_at`.
    pub delayed: bool,
}

/// A text that did not (fully) go out.
#[derive(Debug, Serialize, ToSchema)]
pub struct SmsSendError {
    /// A token the page translates (`agent_not_linked`, `rate_limited`,
    /// `agent_no_number`, `cloudtalk_insufficient_funds`, ...).
    pub error: String,
    /// Messages that went out before one failed.
    pub sent_messages: usize,
    /// Files of the messages that did not go out (set when some did).
    pub unsent_s3_keys: Option<Vec<String>>,
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(SmsSendError {
            error: error.to_string(),
            sent_messages: 0,
            unsent_s3_keys: None,
        }),
    )
        .into_response()
}

/// A key the company's uploads can have: no traversal, safe characters.
fn valid_key(company_id: i32, key: &str) -> bool {
    let prefix = format!("sms-attachments/{company_id}/");
    key.len() > prefix.len()
        && key.starts_with(&prefix)
        && !key.contains("..")
        && !key.contains("//")
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

/// Checks the request; the error token when it cannot be sent.
pub fn check(user: &SessionUser, request: &SendSmsRequest) -> Result<(), &'static str> {
    let images_enabled = std::env::var("SMS_IMAGES_ENABLED").map_or(true, |value| {
        !matches!(
            value.trim().to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        )
    });
    for file in &request.attachments {
        if !valid_key(user.company_id, &file.s3_key)
            || !STORED_TYPES.contains(&file.content_type.as_str())
        {
            return Err("invalid_attachment");
        }
    }
    if !request.attachments.is_empty() && !images_enabled {
        return Err("images_disabled");
    }
    if request.attachments.len() > MAX_ATTACHMENTS {
        return Err("too_many_images");
    }
    let phone = &request.phone_digits;
    if !(10..=15).contains(&phone.len()) || !phone.chars().all(|c| c.is_ascii_digit()) {
        return Err("invalid_phone");
    }
    let text = request.text.trim();
    if text.is_empty() && request.attachments.is_empty() {
        return Err("empty_message");
    }
    if text.chars().count() > MAX_TEXT {
        return Err("text_too_long");
    }
    Ok(())
}

/// Ten sends a minute per user (per Lambda instance).
fn take_send_slot(state: &AppState, user_id: i32) -> bool {
    let limit = std::env::var("SMS_SEND_RATE_LIMIT_PER_MIN")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(10);
    let mut slots = state
        .sms_sends
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let window = std::time::Duration::from_secs(60);
    slots.retain(|_, sends: &mut VecDeque<Instant>| {
        sends.retain(|at| now.duration_since(*at) < window);
        !sends.is_empty()
    });
    let sends = slots.entry(user_id).or_default();
    let allowed = sends.len() < limit;
    if allowed {
        sends.push_back(now);
    }
    drop(slots);
    allowed
}

/// Stores the text for the delayed-message job.
async fn schedule(
    pool: &MySqlPool,
    user: &SessionUser,
    provider: SmsProvider,
    request: &SendSmsRequest,
    send_at: DateTime<Utc>,
) -> ApiResult<()> {
    let now = Utc::now();
    if send_at <= now {
        return Err(ApiError::BadRequest("Choose a future time".into()));
    }
    if send_at - now > Duration::days(MAX_DELAY_DAYS) {
        return Err(ApiError::BadRequest(
            "Send time must be within 90 days".into(),
        ));
    }
    // The CRM's job reads the composer's shape (camelCase).
    let attachments = (!request.attachments.is_empty()).then(|| {
        serde_json::Value::Array(
            request
                .attachments
                .iter()
                .map(|file| {
                    serde_json::json!({
                        "s3Key": file.s3_key,
                        "contentType": file.content_type,
                        "filename": file.filename,
                        "width": file.width,
                        "height": file.height,
                    })
                })
                .collect(),
        )
        .to_string()
    });
    let payload = serde_json::json!({
        "phoneDigits": request.phone_digits,
        "text": request.text.trim(),
        "attachments": attachments,
        "templateId": request.template_id.map(|id| id.to_string()),
    });
    sqlx::query(
        "INSERT INTO delayed_sms (company_id, user_id, customer_id, phone_digits, provider, send_at,
                                  cancel_if_customer_messages, baseline_at, status, payload_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(), 'pending', ?)",
    )
    .bind(user.company_id)
    .bind(user.id)
    .bind(request.customer_id.filter(|id| *id > 0))
    .bind(&request.phone_digits)
    .bind(provider.as_str())
    .bind(send_at.naive_utc())
    .bind(request.cancel_if_customer_messages)
    .bind(payload.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Counts a use of the SMS template (best effort).
async fn record_template_usage(
    pool: &MySqlPool,
    user: &SessionUser,
    provider: SmsProvider,
    template_id: Option<i32>,
) {
    let Some(template_id) = template_id.filter(|id| *id > 0) else {
        return;
    };
    let kind = match provider {
        SmsProvider::Cloudtalk => "cloudtalk_sms",
        SmsProvider::Ringcentral => "ringcentral_sms",
    };
    let result = sqlx::query(
        "INSERT INTO user_template_usage (user_id, template_type, template_id, usage_count, last_used_at)
         SELECT ?, ?, t.id, 1, NOW() FROM cloudtalk_sms_templates t
         WHERE t.id = ? AND t.company_id = ? AND t.deleted_at IS NULL
           AND (t.user_id IS NULL OR t.user_id = ?)
         ON DUPLICATE KEY UPDATE usage_count = usage_count + 1, last_used_at = NOW()",
    )
    .bind(user.id)
    .bind(kind)
    .bind(template_id)
    .bind(user.company_id)
    .bind(user.id)
    .execute(pool)
    .await;
    if let Err(err) = result {
        tracing::warn!(?err, "template usage not recorded");
    }
}

fn failure_response(failure: SmsFailure) -> Response {
    let status = if failure.status == 400 {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    let partly = !failure.sent_ids.is_empty();
    (
        status,
        Json(SmsSendError {
            error: failure.error,
            sent_messages: failure.sent_ids.len(),
            unsent_s3_keys: partly.then_some(failure.unsent_s3_keys),
        }),
    )
        .into_response()
}

/// Why a text did not go out.
pub enum Undelivered {
    /// Storage or database failed.
    Api(ApiError),
    /// The webhooks Lambda refused or the provider failed.
    Sms(SmsFailure),
}

/// Sends a checked text now through the webhooks Lambda, marks the thread
/// read for the sender and counts the template; the stored message ids.
pub async fn deliver_now(
    state: &AppState,
    user: &SessionUser,
    provider: SmsProvider,
    agent_id: String,
    request: &SendSmsRequest,
) -> Result<Vec<i64>, Undelivered> {
    let mut attachments = Vec::with_capacity(request.attachments.len());
    for file in &request.attachments {
        let default_name = if file.content_type.starts_with("image/") {
            "image"
        } else {
            "file"
        };
        let filename: String = file.filename.chars().take(255).collect();
        attachments.push(OutgoingSmsFile {
            url: state
                .storage
                .public_url(&file.s3_key)
                .map_err(|err| Undelivered::Api(err.into()))?,
            s3_key: file.s3_key.clone(),
            content_type: file.content_type.clone(),
            filename: if filename.is_empty() {
                default_name.to_string()
            } else {
                filename
            },
            width: file.width,
            height: file.height,
        });
    }
    let outgoing = OutgoingSms {
        provider: provider.as_str().to_string(),
        company_id: user.company_id,
        user_id: user.id,
        agent_id,
        phone_digits: request.phone_digits.clone(),
        text: request.text.trim().to_string(),
        attachments,
        is_automated: false,
        idempotency_key: None,
    };
    let ids = match state.webhooks.send_sms(&outgoing).await {
        Ok(ids) => ids,
        Err(failure) => return Err(Undelivered::Sms(failure)),
    };
    let digits = phone10(&request.phone_digits);
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {} (user_id, company_id, customer_phone_digits, last_read_at)
         VALUES (?, ?, ?, NOW())
         ON DUPLICATE KEY UPDATE last_read_at = GREATEST(last_read_at, NOW())",
        provider.reads_table()
    )))
    .bind(user.id)
    .bind(user.company_id)
    .bind(&digits)
    .execute(&state.pool)
    .await
    .map_err(|err| Undelivered::Api(err.into()))?;
    record_template_usage(&state.pool, user, provider, request.template_id).await;
    Ok(ids)
}

/// Send a text
///
/// Sends a text (with photos or files uploaded beforehand) to a phone
/// through the caller's agent (`CloudTalk`) or extension (`RingCentral`), or
/// schedules it with `send_at`. `CloudTalk` texts carry links to the files;
/// `RingCentral` sends them as MMS, split into several texts when large.
/// Opening the conversation counts as read. Errors come back as a token in
/// `error`.
#[utoipa::path(
    post,
    path = "/v1/sms/{provider}/messages",
    operation_id = "send_sms",
    tag = "sms",
    params(("provider" = SmsProvider, Path, description = "SMS provider")),
    request_body = SendSmsRequest,
    responses(
        (status = 200, description = "Sent or scheduled", body = SentSms),
        (status = 400, description = "Invalid text or attachment", body = SmsSendError),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller has no agent id for the provider", body = SmsSendError),
        (status = 429, description = "Over ten texts a minute", body = SmsSendError),
        (status = 502, description = "The provider refused or failed", body = SmsSendError),
    ),
    security(("session_cookie" = []))
)]
pub async fn send(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(provider): Path<SmsProvider>,
    JsonBody(request): JsonBody<SendSmsRequest>,
) -> ApiResult<Response> {
    let Some(agent_id) = provider.agent_of(&user) else {
        return Ok(refuse(StatusCode::FORBIDDEN, "agent_not_linked"));
    };
    if let Some(send_at) = request.send_at {
        if request.phone_digits.trim().is_empty() {
            return Ok(refuse(StatusCode::BAD_REQUEST, "invalid_phone"));
        }
        schedule(&state.pool, &user, provider, &request, send_at).await?;
        return Ok(Json(SentSms {
            message: None,
            delayed: true,
        })
        .into_response());
    }
    if let Err(token) = check(&user, &request) {
        return Ok(refuse(StatusCode::BAD_REQUEST, token));
    }
    if !take_send_slot(&state, user.id) {
        return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    let ids = match deliver_now(&state, &user, provider, agent_id, &request).await {
        Ok(ids) => ids,
        Err(Undelivered::Api(err)) => return Err(err),
        Err(Undelivered::Sms(failure)) => return Ok(failure_response(failure)),
    };
    let digits = phone10(&request.phone_digits);
    let message = match ids.first() {
        Some(id) => message_by_id(&state.pool, provider, user.company_id, &digits, *id).await?,
        None => None,
    };
    Ok(Json(SentSms {
        message,
        delayed: false,
    })
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::valid_key;

    #[test]
    fn keys_stay_in_the_company_folder() {
        assert!(valid_key(100, "sms-attachments/100/m/f.jpg"));
        assert!(!valid_key(100, "sms-attachments/101/m/f.jpg"));
        assert!(!valid_key(100, "sms-attachments/100/../101/f.jpg"));
        assert!(!valid_key(100, "sms-attachments/100/"));
        assert!(!valid_key(100, "sms-attachments/100/a b.jpg"));
    }
}
