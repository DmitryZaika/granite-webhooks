//! The caller's to-do list.
//!
//! - `GET    /v1/me/todos`              → in their order
//! - `POST   /v1/me/todos`              → add at the top
//! - `PUT    /v1/me/todos/{id}`         → change the text
//! - `PUT    /v1/me/todos/{id}/done`    → tick or untick
//! - `DELETE /v1/me/todos/{id}`         → delete one
//! - `DELETE /v1/me/todos/done`         → delete every ticked one
//! - `PUT    /v1/me/todo-order`         → save a drag-and-drop order
//!
//! Replaces the Remix `api.todoList*` routes.

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

/// A to-do.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct Todo {
    /// The to-do.
    pub id: i32,
    /// The task.
    pub rich_text: String,
    /// Ticked off.
    pub is_done: bool,
    /// Order in the list.
    pub position: Option<i32>,
    /// When it was added.
    pub created_date: Option<DateTime<Utc>>,
}

/// Body of `POST /v1/me/todos` and `PUT /v1/me/todos/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TodoText {
    /// The task, 1 to 255 characters.
    pub rich_text: String,
}

/// Body of `PUT /v1/me/todos/{id}/done`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TodoDone {
    /// Ticked off or not.
    pub done: bool,
}

/// `POST /v1/me/todos`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedTodo {
    /// The new to-do.
    pub id: i32,
}

fn checked(text: &str) -> ApiResult<&str> {
    if text.is_empty() || text.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "rich_text must be 1 to 255 characters".into(),
        ));
    }
    Ok(text)
}

/// List my to-dos
///
/// The caller's to-dos, open ones first unless reordered by hand.
#[crate::openapi::path(
    get,
    path = "/v1/me/todos",
    operation_id = "list_todos",
    tag = "me",
    responses(
        (status = 200, description = "To-dos by position", body = [Todo]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<Todo>>> {
    let todos = sqlx::query_as(
        "SELECT id, rich_text, is_done <> 0 AS is_done, position, created_date FROM todolist
          WHERE user_id = ? ORDER BY position",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(todos))
}

/// Add a to-do
///
/// It goes to the top of the list.
#[crate::openapi::path(
    post,
    path = "/v1/me/todos",
    operation_id = "create_todo",
    tag = "me",
    request_body = TodoText,
    responses(
        (status = 201, description = "Added", body = CreatedTodo),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<TodoText>,
) -> ApiResult<(StatusCode, Json<CreatedTodo>)> {
    let text = checked(&input.rich_text)?;
    let id = sqlx::query(
        "INSERT INTO todolist (rich_text, user_id, position)
         SELECT ?, ?, COALESCE(MIN(position), 0) - 1 FROM todolist WHERE user_id = ?",
    )
    .bind(text)
    .bind(user.id)
    .bind(user.id)
    .execute(&state.pool)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedTodo {
            id: i32::try_from(id).unwrap_or_default(),
        }),
    ))
}

/// The caller's own to-do, or 404.
async fn own(pool: &sqlx::MySqlPool, user_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> =
        sqlx::query_scalar("SELECT id FROM todolist WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// Change a to-do's text
///
/// Only the caller's own to-dos.
#[crate::openapi::path(
    put,
    path = "/v1/me/todos/{id}",
    operation_id = "update_todo",
    tag = "me",
    params(("id" = i32, Path, description = "To-do id")),
    request_body = TodoText,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's to-do", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TodoText>,
) -> ApiResult<StatusCode> {
    let text = checked(&input.rich_text)?;
    own(&state.pool, user.id, id).await?;
    sqlx::query("UPDATE todolist SET rich_text = ? WHERE id = ? AND user_id = ?")
        .bind(text)
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Tick or untick a to-do
///
/// A ticked to-do moves to the bottom; an unticked one goes back above the
/// ticked ones.
#[crate::openapi::path(
    put,
    path = "/v1/me/todos/{id}/done",
    operation_id = "set_todo_done",
    tag = "me",
    params(("id" = i32, Path, description = "To-do id")),
    request_body = TodoDone,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's to-do", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TodoDone>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    own(pool, user.id, id).await?;
    let position: i32 = if input.done {
        sqlx::query_scalar("SELECT COALESCE(MAX(position), -1) + 1 FROM todolist WHERE user_id = ?")
            .bind(user.id)
            .fetch_one(pool)
            .await?
    } else {
        let first_done: Option<i32> = sqlx::query_scalar(
            "SELECT MIN(position) FROM todolist WHERE user_id = ? AND is_done = 1 AND id <> ?",
        )
        .bind(user.id)
        .bind(id)
        .fetch_one(pool)
        .await?;
        match first_done {
            Some(first) => first - 1,
            None => {
                sqlx::query_scalar(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM todolist
                      WHERE user_id = ? AND is_done = 0 AND id <> ?",
                )
                .bind(user.id)
                .bind(id)
                .fetch_one(pool)
                .await?
            }
        }
    };
    sqlx::query("UPDATE todolist SET is_done = ?, position = ? WHERE id = ? AND user_id = ?")
        .bind(input.done)
        .bind(position)
        .bind(id)
        .bind(user.id)
        .execute(pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a to-do
///
/// Someone else's to-do is left alone.
#[crate::openapi::path(
    delete,
    path = "/v1/me/todos/{id}",
    operation_id = "delete_todo",
    tag = "me",
    params(("id" = i32, Path, description = "To-do id")),
    responses(
        (status = 204, description = "Deleted (or already gone)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM todolist WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete ticked to-dos
///
/// Clears every to-do the caller ticked off.
#[crate::openapi::path(
    delete,
    path = "/v1/me/todos/done",
    operation_id = "delete_done_todos",
    tag = "me",
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM todolist WHERE user_id = ? AND is_done = 1")
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Order my to-dos
///
/// The caller's to-dos in the order given (others keep their place).
#[crate::openapi::path(
    put,
    path = "/v1/me/todo-order",
    operation_id = "order_todos",
    tag = "me",
    request_body = Vec<i32>,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn order(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(ids): JsonBody<Vec<i32>>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    for (position, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE todolist SET position = ? WHERE id = ? AND user_id = ?")
            .bind(i32::try_from(position).unwrap_or(i32::MAX))
            .bind(id)
            .bind(user.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
