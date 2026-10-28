//! Notes on the sold project behind an event (`sale_project_notes`).
//!
//! A template maker tells the installers or the sales rep what they found on
//! site. The notes belong to the sale, so its template and install events
//! show the same ones.

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

const AUDIENCES: [&str; 3] = ["installers", "sales_rep", "everyone"];

/// A note on the project.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ProjectNote {
    /// The note.
    pub id: i32,
    /// `installers`, `sales_rep` or `everyone`.
    pub audience: String,
    /// The text.
    pub content: String,
    /// When it was written.
    pub created_at: DateTime<Utc>,
    /// Author's name.
    pub created_by: Option<String>,
    /// Whether the caller wrote it (only they can delete it).
    pub own: bool,
}

/// Body of `POST /v1/events/{id}/project-notes`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ProjectNoteInput {
    /// 1 to 2000 characters (trimmed).
    pub content: String,
    /// `installers`, `sales_rep` or `everyone` (the default).
    pub audience: Option<String>,
}

/// The event's sale, when the event is the company's and has one.
async fn event_sale(pool: &MySqlPool, company_id: i32, event_id: i32) -> ApiResult<i32> {
    let sale: Option<Option<i32>> = sqlx::query_scalar(
        "SELECT e.sale_id FROM events e JOIN users u ON e.created_user_id = u.id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    sale.flatten().ok_or(ApiError::NotFound)
}

/// List the project's notes
///
/// The notes on the event's sale, oldest first.
#[crate::openapi::path(
    get,
    path = "/v1/events/{id}/project-notes",
    operation_id = "list_project_notes",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    responses(
        (status = 200, description = "Notes", body = Vec<ProjectNote>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event with a sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<ProjectNote>>> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let notes = sqlx::query_as(
        "SELECT n.id, n.audience, n.content, n.created_at, u.name AS created_by,
                n.created_by = ? AS own
           FROM sale_project_notes n LEFT JOIN users u ON u.id = n.created_by
          WHERE n.company_id = ? AND n.sale_id = ? AND n.deleted_at IS NULL
          ORDER BY n.created_at, n.id",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(sale_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(notes))
}

/// Leave a note on the project
///
/// Signed with the caller; every event of the sale shows it.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/project-notes",
    operation_id = "add_project_note",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = ProjectNoteInput,
    responses(
        (status = 201, description = "Added"),
        (status = 400, description = "Empty, too long or an unknown audience", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event with a sale in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<ProjectNoteInput>,
) -> ApiResult<StatusCode> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let content = input.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("Write the note first".into()));
    }
    if content.chars().count() > 2000 {
        return Err(ApiError::BadRequest(
            "A note must be 2000 characters or less".into(),
        ));
    }
    let audience = input.audience.as_deref().unwrap_or("everyone");
    if !AUDIENCES.contains(&audience) {
        return Err(ApiError::BadRequest(
            "A note is for installers, the sales rep or everyone".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO sale_project_notes (company_id, sale_id, audience, content, created_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(sale_id)
    .bind(audience)
    .bind(content)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::CREATED)
}

/// Delete my project note
///
/// Only the author can delete a note.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/project-notes/{note_id}",
    operation_id = "delete_project_note",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("note_id" = i32, Path, description = "Note id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's note on this project", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let sale_id = event_sale(&state.pool, user.company_id, id).await?;
    let deleted = sqlx::query(
        "UPDATE sale_project_notes SET deleted_at = NOW()
          WHERE id = ? AND company_id = ? AND sale_id = ? AND created_by = ?
            AND deleted_at IS NULL",
    )
    .bind(note_id)
    .bind(user.company_id)
    .bind(sale_id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
