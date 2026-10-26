//! Every time the recipient opened a sent email (read receipts).

use super::sql::{CLIENT_OPEN_MIN_DELAY, can_read_thread};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use sqlx::FromRow;

/// Most opens returned.
const MAX_READS: i64 = 200;

/// One time the recipient opened the email.
#[derive(Debug, Serialize, FromRow, ToSchema)]
pub struct EmailRead {
    /// When it was opened (ISO 8601, UTC).
    pub read_at: String,
    /// The reader's mail app or browser (`User-Agent`), when known.
    pub user_agent: Option<String>,
}

/// List every open of a sent email
///
/// Each time the recipient opened one of the company's emails, newest
/// first (at most 200). Opens in the first 5 seconds after sending are left
/// out: those are mail scanners and image proxies, not the client. The
/// latest of these is the `client_read_at` the mailbox lists show.
#[crate::openapi::path(
    get,
    path = "/v1/emails/{id}/reads",
    operation_id = "list_email_reads",
    tag = "emails",
    params(("id" = i32, Path, description = "Email id")),
    responses(
        (status = 200, description = "Opens, newest first (empty when never opened)", body = Vec<EmailRead>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such email in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn email_reads(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<EmailRead>>> {
    let email: Option<(Option<i32>, Option<String>)> =
        sqlx::query_as("SELECT company_id, thread_id FROM emails WHERE id = ? LIMIT 1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
    let allowed = match email {
        Some((Some(company), _)) => company == user.company_id,
        Some((None, Some(thread))) => can_read_thread(&thread, user.company_id)
            .query()
            .fetch_optional(&state.pool)
            .await?
            .is_some(),
        _ => false,
    };
    if !allowed {
        return Err(ApiError::NotFound);
    }
    let reads: Vec<EmailRead> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT DATE_FORMAT(er.read_at, '%Y-%m-%dT%H:%i:%sZ') AS read_at, er.user_agent
           FROM email_reads er JOIN emails e ON e.message_id = er.message_id
          WHERE e.id = ? AND {CLIENT_OPEN_MIN_DELAY}
          ORDER BY er.read_at DESC
          LIMIT {MAX_READS}"
    )))
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(reads))
}
