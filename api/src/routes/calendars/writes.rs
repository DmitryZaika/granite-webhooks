//! Admin > Schedule: add, edit, hide and delete calendars.
//! Remix: the action of `api.calendars.ts`.

use super::queries::SYSTEM_CALENDARS;
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Body of `POST /v1/calendars` and `PUT /v1/calendars/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CalendarInput {
    /// 1 to 100 characters; ignored for built-in calendars.
    pub name: String,
    /// Color name, e.g. `blue` (the default).
    #[serde(default)]
    pub color: Option<String>,
    /// Hidden from everyone's schedule (create only).
    #[serde(default)]
    pub hidden: bool,
}

/// Body of `PUT /v1/calendars/{id}/hidden`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CalendarHidden {
    /// Hidden from everyone's schedule.
    pub hidden: bool,
}

/// Path of `PUT /v1/calendars/{id}/hidden-users/{user_id}`.
#[derive(Debug, Deserialize, crate::openapi::IntoParams)]
#[into_params(parameter_in = Path)]
pub struct HiddenUserPath {
    /// Calendar id.
    pub id: i32,
    /// User id.
    pub user_id: i32,
}

/// `POST /v1/calendars`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedCalendar {
    /// The new calendar.
    pub id: u64,
}

/// Trimmed name and color, checked.
fn checked(input: &CalendarInput) -> ApiResult<(&str, &str)> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    if name.chars().count() > 100 {
        return Err(ApiError::BadRequest(
            "Name must be 100 characters or less".into(),
        ));
    }
    let color = input
        .color
        .as_deref()
        .map(str::trim)
        .filter(|color| !color.is_empty())
        .unwrap_or("blue");
    if color.chars().count() > 50 {
        return Err(ApiError::BadRequest("Invalid color".into()));
    }
    Ok((name, color))
}

/// The slug of a live calendar of the company (`None` for a custom one), or 404.
async fn find(pool: &MySqlPool, company_id: i32, id: i32) -> ApiResult<Option<String>> {
    let found: Option<Option<String>> = sqlx::query_scalar(
        "SELECT slug FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)
}

fn is_system(slug: Option<&str>) -> bool {
    slug.is_some_and(|slug| SYSTEM_CALENDARS.iter().any(|(s, _, _)| *s == slug))
}

/// Add a calendar
///
/// The built-in calendars' names are reserved.
#[crate::openapi::path(
    post,
    path = "/v1/calendars",
    operation_id = "create_calendar",
    tag = "schedule",
    request_body = CalendarInput,
    responses(
        (status = 201, description = "Added", body = CreatedCalendar),
        (status = 400, description = "Missing or reserved name, or bad color", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<CalendarInput>,
) -> ApiResult<(StatusCode, Json<CreatedCalendar>)> {
    let (name, color) = checked(&input)?;
    if SYSTEM_CALENDARS
        .iter()
        .any(|(_, reserved, _)| reserved.eq_ignore_ascii_case(name))
    {
        return Err(ApiError::BadRequest(
            "That calendar name is reserved".into(),
        ));
    }
    let id = sqlx::query(
        "INSERT INTO calendars (company_id, name, color, created_by, is_hidden) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user.company_id)
    .bind(name)
    .bind(color)
    .bind(user.id)
    .bind(input.hidden)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((StatusCode::CREATED, Json(CreatedCalendar { id })))
}

/// Edit a calendar
///
/// Saves the name and color; a built-in calendar only changes color.
#[crate::openapi::path(
    put,
    path = "/v1/calendars/{id}",
    operation_id = "update_calendar",
    tag = "schedule",
    params(("id" = i32, Path, description = "Calendar id")),
    request_body = CalendarInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Missing name or bad color", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such calendar in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CalendarInput>,
) -> ApiResult<StatusCode> {
    let slug = find(&state.pool, user.company_id, id).await?;
    let (name, color) = checked(&input)?;
    let query = if is_system(slug.as_deref()) {
        sqlx::query("UPDATE calendars SET color = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(color)
            .bind(id)
    } else {
        sqlx::query(
            "UPDATE calendars SET name = ?, color = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
        )
        .bind(name)
        .bind(color)
        .bind(id)
    };
    query.execute(&state.pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Hide or show a calendar
///
/// A hidden calendar is left off everyone's schedule.
#[crate::openapi::path(
    put,
    path = "/v1/calendars/{id}/hidden",
    operation_id = "set_calendar_hidden",
    tag = "schedule",
    params(("id" = i32, Path, description = "Calendar id")),
    request_body = CalendarHidden,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such calendar in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_hidden(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CalendarHidden>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, id).await?;
    sqlx::query("UPDATE calendars SET is_hidden = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(input.hidden)
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a calendar
///
/// Removes a calendar the company added; built-in ones stay.
#[crate::openapi::path(
    delete,
    path = "/v1/calendars/{id}",
    operation_id = "delete_calendar",
    tag = "schedule",
    params(("id" = i32, Path, description = "Calendar id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "A built-in calendar", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such calendar in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let slug = find(&state.pool, user.company_id, id).await?;
    if is_system(slug.as_deref()) {
        return Err(ApiError::BadRequest(
            "System calendars cannot be deleted".into(),
        ));
    }
    sqlx::query("UPDATE calendars SET deleted_at = NOW() WHERE id = ?")
        .bind(id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Hide a calendar from one user, or show it again
///
/// A calendar hidden from a user is left off their schedule, with its events.
#[crate::openapi::path(
    put,
    path = "/v1/calendars/{id}/hidden-users/{user_id}",
    operation_id = "set_calendar_hidden_for_user",
    tag = "schedule",
    params(HiddenUserPath),
    request_body = CalendarHidden,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such calendar or user in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_hidden_for_user(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(path): Path<HiddenUserPath>,
    JsonBody(input): JsonBody<CalendarHidden>,
) -> ApiResult<StatusCode> {
    find(&state.pool, user.company_id, path.id).await?;
    let member: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(path.user_id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    if member.is_none() {
        return Err(ApiError::NotFound);
    }
    let query = if input.hidden {
        "INSERT IGNORE INTO calendar_hidden_users (calendar_id, user_id, company_id) VALUES (?, ?, ?)"
    } else {
        "DELETE FROM calendar_hidden_users WHERE calendar_id = ? AND user_id = ? AND company_id = ?"
    };
    sqlx::query(query)
        .bind(path.id)
        .bind(path.user_id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
