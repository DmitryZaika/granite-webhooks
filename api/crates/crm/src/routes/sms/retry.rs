//! Retrying a text that failed: `POST /v1/sms/{provider}/messages/{id}/retry`.
//!
//! The failed row is sent again as it was stored (text and files) by the
//! rep who sent it, and the new attempt takes its place in the conversation.
//! While it goes out the row shows as sending (`pending`, dated now, so the
//! stale-send upkeep leaves it alone and a second retry cannot claim it).

use super::provider::{SmsProvider, phone10};
use super::send::{
    SendSmsRequest, SentSms, SmsFile, SmsSendError, Undelivered, check, deliver_now,
    failure_response, refuse, seat, take_send_slot,
};
use super::thread::message_by_id;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sqlx::MySqlPool;

#[derive(sqlx::FromRow)]
struct FailedRow {
    phone_digits: String,
    text: String,
}

/// The caller's own failed outbound text.
async fn failed_row(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    user_id: i32,
    id: i32,
) -> Result<Option<FailedRow>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(phone_digits, recipient10, CAST(recipient AS CHAR)) AS phone_digits, text
           FROM {} WHERE id = ? AND company_id = ? AND sender_user_id = ?
            AND direction = 'outbound' AND status = 'failed'",
        provider.sms_table()
    )))
    .bind(id)
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// The row's files, in order, as the composer sends them.
async fn stored_files(
    pool: &MySqlPool,
    provider: SmsProvider,
    id: i32,
) -> Result<Vec<SmsFile>, sqlx::Error> {
    let (table, column) = provider.attachments();
    let rows: Vec<(String, String, String, Option<i32>, Option<i32>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT s3_key, content_type, filename, width, height
               FROM {table} WHERE {column} = ? ORDER BY position, id"
        )))
        .bind(id)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(s3_key, content_type, filename, width, height)| SmsFile {
            s3_key,
            content_type,
            filename,
            width,
            height,
        })
        .collect())
}

/// Marks the failed row as going out; `false` when it is no longer failed
/// (another retry took it).
async fn claim(pool: &MySqlPool, provider: SmsProvider, id: i32) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = 'pending', created_date = CURRENT_TIMESTAMP
          WHERE id = ? AND status = 'failed'",
        provider.sms_table()
    )))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// After a failed attempt: the provider's new failed row replaces the old
/// one; without one (nothing reached the provider) the old one is failed
/// again.
async fn settle_failure(
    pool: &MySqlPool,
    provider: SmsProvider,
    company_id: i32,
    user_id: i32,
    id: i32,
    phone: &str,
) -> Result<(), sqlx::Error> {
    let table = provider.sms_table();
    let newer: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table}
          WHERE company_id = ? AND sender_user_id = ? AND direction = 'outbound'
            AND status = 'failed' AND id > ? AND phone_digits = ?
          LIMIT 1"
    )))
    .bind(company_id)
    .bind(user_id)
    .bind(id)
    .bind(phone)
    .fetch_optional(pool)
    .await?;
    let sql = if newer.is_some() {
        format!("DELETE FROM {table} WHERE id = ? AND status = 'pending'")
    } else {
        format!("UPDATE {table} SET status = 'failed' WHERE id = ? AND status = 'pending'")
    };
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Retry a failed text
///
/// Sends one of the caller's failed texts again, with its files, and
/// replaces it with the new attempt: the sent text, or the new failure.
/// Errors come back as a token in `error`, as when sending.
#[crate::openapi::path(
    post,
    path = "/v1/sms/{provider}/messages/{id}/retry",
    operation_id = "retry_sms",
    tag = "sms",
    params(
        ("provider" = SmsProvider, Path, description = "SMS provider"),
        ("id" = i32, Path, description = "The failed message"),
    ),
    responses(
        (status = 200, description = "Sent", body = SentSms),
        (status = 400, description = "The stored text cannot be sent", body = SmsSendError),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller has no agent id for the provider", body = SmsSendError),
        (status = 404, description = "Not a failed text of the caller's", body = ErrorBody),
        (status = 429, description = "Over ten texts a minute", body = SmsSendError),
        (status = 502, description = "The provider refused or failed", body = SmsSendError),
    ),
    security(("session_cookie" = []))
)]
pub async fn retry(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((provider, id)): Path<(SmsProvider, i32)>,
) -> ApiResult<Response> {
    let pool = &state.pool;
    let Some(seat) = seat(pool, provider, &user).await? else {
        return Ok(refuse(StatusCode::FORBIDDEN, "agent_not_linked"));
    };
    let Some(row) = failed_row(pool, provider, user.company_id, user.id, id).await? else {
        return Err(ApiError::NotFound);
    };
    let request = SendSmsRequest {
        phone_digits: row.phone_digits,
        text: row.text,
        attachments: stored_files(pool, provider, id).await?,
        template_id: None,
        customer_id: None,
        send_at: None,
        cancel_if_customer_messages: false,
    };
    if let Err(token) = check(&user, &request) {
        return Ok(refuse(StatusCode::BAD_REQUEST, token));
    }
    if !take_send_slot(&state, user.id) {
        return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    if !claim(pool, provider, id).await? {
        return Err(ApiError::NotFound);
    }
    let digits = phone10(&request.phone_digits);
    let ids = match deliver_now(&state, &user, provider, &seat, &request).await {
        Ok(ids) => ids,
        Err(undelivered) => {
            settle_failure(pool, provider, user.company_id, user.id, id, &digits).await?;
            return match undelivered {
                Undelivered::Api(err) => Err(err),
                Undelivered::Sms(failure) => Ok(failure_response(failure)),
            };
        }
    };
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {} WHERE id = ? AND status = 'pending'",
        provider.sms_table()
    )))
    .bind(id)
    .execute(pool)
    .await?;
    let message = match ids.first() {
        Some(id) => message_by_id(pool, provider, user.company_id, &digits, *id).await?,
        None => None,
    };
    Ok(Json(SentSms {
        message,
        delayed: false,
    })
    .into_response())
}
