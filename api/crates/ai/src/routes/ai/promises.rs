//! The rep's AI promises (`crate::promises`), for the CRM:
//!
//! - `GET   /v1/ai/promises` → the caller's open promises; with a customer,
//!   phone or email, only that customer's
//! - `PATCH /v1/ai/promises/{id}` → kept (`done`), not wanted (`dismissed`)
//!   or open again
//!
//! Only for users AI promises is on for (`users.ai_promises`).

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Query of `GET /v1/ai/promises`. With more than one, a promise matching
/// any of them is the customer's.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PromisesQuery {
    /// Only this customer's promises.
    pub customer_id: Option<i32>,
    /// Only promises to this phone (its last 10 digits count).
    pub phone_digits: Option<String>,
    /// Only promises to this email.
    pub customer_email: Option<String>,
}

/// A promise the rep made a customer.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AiPromise {
    pub id: u64,
    /// Where the rep said it: `call`, `sms` or `email`.
    pub source: String,
    /// When the rep said it.
    pub promised_at: DateTime<Utc>,
    pub customer_id: Option<i32>,
    pub customer_name: Option<String>,
    /// The customer's phone (10 digits), for a call or a text.
    pub customer_digits: Option<String>,
    /// The customer's email, for an email.
    pub customer_email: Option<String>,
    /// What to do, e.g. "Send the inventory link".
    pub title: String,
    /// The rep's words.
    pub said: String,
    /// When it was promised for.
    pub due_at: Option<DateTime<Utc>>,
    /// `sms` or `email` when a message keeps it.
    pub channel: Option<String>,
    /// That message, ready to send (null when the AI could not write it).
    pub draft: Option<String>,
}

/// Open promises.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromises {
    /// Newest first.
    pub promises: Vec<AiPromise>,
}

/// Body of `PATCH /v1/ai/promises/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PromiseUpdate {
    /// `done` (kept), `dismissed` (not a promise, or not wanted) or `open`.
    pub status: String,
}

async fn require_enabled(state: &AppState, user: &SessionUser) -> ApiResult<()> {
    let on: Option<bool> = sqlx::query_scalar("SELECT ai_promises FROM users WHERE id = ?")
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
    if on == Some(true) {
        Ok(())
    } else {
        Err(ApiError::Denied(
            "AI promises are not switched on for your account".into(),
        ))
    }
}

/// List the caller's open promises
///
/// What the caller promised customers on calls, texts and emails and has not
/// done yet, newest first, each with the message that keeps it when a text
/// or email can. With a customer, phone or email: only that customer's.
#[crate::openapi::path(
    get,
    path = "/v1/ai/promises",
    operation_id = "list_ai_promises",
    tag = "ai",
    params(PromisesQuery),
    responses(
        (status = 200, description = "Open promises", body = AiPromises),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or AI promises are not on for them", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<PromisesQuery>,
) -> ApiResult<Json<AiPromises>> {
    require_enabled(&state, &user).await?;
    let digits: String = query
        .phone_digits
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let digits = (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string());
    let email = query
        .customer_email
        .as_deref()
        .map(|email| email.trim().to_lowercase())
        .filter(|email| email.contains('@'));
    let customer_id = query.customer_id.filter(|id| *id > 0);
    let filtered = customer_id.is_some() || digits.is_some() || email.is_some();
    let promises: Vec<AiPromise> = sqlx::query_as(
        "SELECT p.id, CAST(p.source AS CHAR) AS source, p.promised_at, p.customer_id,
                c.name AS customer_name, p.customer_digits, p.customer_email, p.title, p.said,
                p.due_at, CAST(p.channel AS CHAR) AS channel, p.draft
           FROM ai_promises p
           LEFT JOIN customers c ON c.id = p.customer_id AND c.deleted_at IS NULL
          WHERE p.user_id = ? AND p.company_id = ? AND p.status = 'open'
            AND (? = 0 OR p.customer_id = ? OR p.customer_digits = ? OR p.customer_email = ?)
          ORDER BY p.promised_at DESC, p.id DESC
          LIMIT 200",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(filtered)
    .bind(customer_id)
    .bind(digits)
    .bind(email)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(AiPromises { promises }))
}

/// Close or reopen a promise
///
/// `done` once the rep kept it (sending its message does this), `dismissed`
/// when it was not a promise or is not wanted, `open` to bring it back.
#[crate::openapi::path(
    patch,
    path = "/v1/ai/promises/{id}",
    operation_id = "update_ai_promise",
    tag = "ai",
    params(("id" = u64, Path, description = "Promise id")),
    request_body = PromiseUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Status is not done, dismissed or open", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or AI promises are not on for them", body = ErrorBody),
        (status = 404, description = "Not one of the caller's promises", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<u64>,
    JsonBody(input): JsonBody<PromiseUpdate>,
) -> ApiResult<StatusCode> {
    require_enabled(&state, &user).await?;
    let status = input.status.trim();
    if !matches!(status, "done" | "dismissed" | "open") {
        return Err(ApiError::BadRequest(
            "Status must be done, dismissed or open".into(),
        ));
    }
    let found: Option<u64> = sqlx::query_scalar(
        "SELECT id FROM ai_promises WHERE id = ? AND user_id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query(
        "UPDATE ai_promises
            SET status = ?, closed_at = IF(? = 'open', NULL, UTC_TIMESTAMP())
          WHERE id = ?",
    )
    .bind(status)
    .bind(status)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
