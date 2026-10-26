//! Super admin review of CRM manual corrections from the AI chat.
//!
//! - `GET  /v1/superuser/manual-feedback`               → feedback waiting for review
//! - `POST /v1/superuser/manual-feedback/{id}/resolve`  → approve the manual change, or just mark reviewed
//!
//! Users rate the chat's answers about the CRM (`POST /v1/ai/chat/feedback`);
//! approved or escalated threads wait here. Approving writes the proposed
//! section text to `crm_manual_overrides`, which the chat reads on every
//! answer. Replaces the Remix `admin.superuser-dashboard.instruction-corrections`
//! loader and action.

use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(pending_reviews))
        .routes(routes!(resolve_review))
}

/// A thread waiting for review.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct FeedbackReview {
    /// Feedback id.
    pub id: i32,
    /// Who rated the answer, or null.
    pub user_id: Option<i32>,
    /// Their company, or null.
    pub company_id: Option<i32>,
    /// Company name, or null.
    pub company_name: Option<String>,
    /// User name, or null.
    pub user_name: Option<String>,
    /// The conversation as the chat saved it (`question`, `originalAnswer`,
    /// `corrections`, or `dialog`).
    #[sqlx(json)]
    #[schema(value_type = Object)]
    pub history: Value,
    /// What the user wrote, or null.
    pub feedback_text: Option<String>,
    /// `user_approved` or `escalated`.
    pub status: String,
    /// The manual section to change, or null.
    pub manual_section_id: Option<String>,
    /// The section's proposed new text, or null.
    pub proposed_manual_change: Option<String>,
    /// When it was sent (ISO 8601), or null.
    pub created_at: Option<DateTime<Utc>>,
}

/// List manual feedback to review
///
/// Unresolved threads users approved or escalated, newest first.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/manual-feedback",
    operation_id = "list_manual_feedback_reviews",
    tag = "superuser",
    responses(
        (status = 200, description = "Threads to review", body = [FeedbackReview]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pending_reviews(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<FeedbackReview>>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let rows = sqlx::query_as(
        "SELECT mf.id, mf.user_id, mf.company_id, c.name AS company_name, u.name AS user_name,
                mf.history, mf.feedback_text, CAST(mf.status AS CHAR) AS status,
                mf.manual_section_id, mf.proposed_manual_change, mf.created_at
           FROM message_feedback mf
           LEFT JOIN company c ON c.id = mf.company_id
           LEFT JOIN users u ON u.id = mf.user_id
          WHERE mf.resolved_at IS NULL AND mf.status IN ('user_approved', 'escalated')
          ORDER BY mf.created_at DESC, mf.id DESC",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

/// Body of `POST /v1/superuser/manual-feedback/{id}/resolve`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct Resolve {
    /// Write the proposed section text into the manual (when the thread has
    /// one); false only marks it reviewed.
    pub apply_manual_change: bool,
}

/// Resolve a manual feedback thread
///
/// Marks the thread reviewed; with `apply_manual_change` its proposed
/// section text replaces that section of the CRM manual for the chat.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/manual-feedback/{id}/resolve",
    operation_id = "resolve_manual_feedback_review",
    tag = "superuser",
    params(("id" = i32, Path, description = "Feedback id")),
    request_body = Resolve,
    responses(
        (status = 204, description = "Resolved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such feedback", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn resolve_review(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<Resolve>,
) -> ApiResult<StatusCode> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let feedback: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT manual_section_id, proposed_manual_change FROM message_feedback WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;
    let (section, change) = feedback.ok_or(ApiError::NotFound)?;
    let mut tx = state.pool.begin().await?;
    if input.apply_manual_change
        && let (Some(section), Some(change)) = (section, change)
        && !section.is_empty()
        && !change.is_empty()
    {
        sqlx::query(
            "INSERT INTO crm_manual_overrides (section_id, content, feedback_id) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE content = VALUES(content), feedback_id = VALUES(feedback_id)",
        )
        .bind(section)
        .bind(change)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE message_feedback SET resolved_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
