//! Tasks, notes and files of a project. Anyone who may open the project adds
//! them; a task may go to anyone in the company, who is told at once and
//! again when it is due.

use super::access::Viewer;
use super::schemas::{NewProjectFiles, NoteInput, TaskInput, TaskUpdate};
use super::service::visible_project;
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::created::CreatedId;
use crate::routes::deals::service::reminder_due_at;
use crate::routes::uploads::schemas::UploadFolder;
use crate::routes::uploads::{attach, remove};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{NaiveDate, NaiveTime};
use lambda_http::tracing;
use sqlx::MySqlConnection;

/// The bell: a task was given to you.
pub const TASK_ASSIGNED: &str = "project_task_assigned";
/// The bell, at the task's deadline.
pub const TASK_DUE: &str = "project_task_reminder";

/// Date-only deadlines are reminded at this store time on the day.
const DATE_ONLY_REMINDER: &str = "09:00:00";

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// `YYYY-MM-DD` or `YYYY-MM-DD[T ]HH:MM[:SS]`, as stored (`YYYY-MM-DD HH:MM:SS`).
fn parse_deadline(raw: &str) -> ApiResult<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let invalid = || ApiError::BadRequest("Pick a valid due date".into());
    let date = NaiveDate::parse_from_str(raw.get(..10).ok_or_else(invalid)?, "%Y-%m-%d")
        .map_err(|_| invalid())?;
    if raw.len() == 10 {
        return Ok(Some(format!("{date} 00:00:00")));
    }
    let separator = raw.get(10..11).ok_or_else(invalid)?;
    if separator != "T" && separator != " " {
        return Err(invalid());
    }
    let time = raw.get(11..).ok_or_else(invalid)?;
    let time = NaiveTime::parse_from_str(time, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(time, "%H:%M"))
        .map_err(|_| invalid())?;
    Ok(Some(format!("{date} {}", time.format("%H:%M:%S"))))
}

/// When a stored deadline is reminded: its time, or 9 AM store time on a
/// date with no time.
fn reminder_time(stored: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (date, time) = stored.split_once(' ')?;
    let time = if time == "00:00:00" {
        DATE_ONLY_REMINDER
    } else {
        time
    };
    reminder_due_at(&format!("{date}T{time}"))
}

fn priority(raw: Option<&str>) -> ApiResult<&'static str> {
    match raw.map(str::trim).unwrap_or("medium") {
        "low" => Ok("low"),
        "" | "medium" => Ok("medium"),
        "high" => Ok("high"),
        _ => Err(ApiError::BadRequest(
            "Priority is low, medium or high".into(),
        )),
    }
}

async fn require_colleague(
    conn: &mut MySqlConnection,
    company_id: i32,
    user_id: i32,
) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ? AND company_id = ? AND is_deleted = 0",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    found
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("Pick someone in your company".into()))
}

fn actor_name(user: &SessionUser) -> Option<String> {
    let name = user.name.as_deref().unwrap_or_default().trim();
    (!name.is_empty()).then(|| clip(name, 255))
}

/// Tells the assignee (not the actor) and schedules the due reminder.
/// Logged, never returned: the task is saved.
async fn notify(
    state: &AppState,
    user: &SessionUser,
    project_id: i32,
    task_id: i32,
    name: &str,
    assignee: Option<i32>,
    deadline: Option<&str>,
) {
    let result: sqlx::Result<()> = async {
        let message = clip(name, 255);
        if let Some(assignee) = assignee.filter(|id| *id != user.id) {
            let id = sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, project_task_id, message, notification_type, actor_name, due_at)
                 VALUES (?, ?, ?, ?, ?, ?, NOW())",
            )
            .bind(assignee)
            .bind(project_id)
            .bind(task_id)
            .bind(&message)
            .bind(TASK_ASSIGNED)
            .bind(actor_name(user))
            .execute(&state.pool)
            .await?
            .last_insert_id();
            push_notification_logged(state, i64::try_from(id).unwrap_or_default()).await;
        }
        if let Some(due) = deadline.and_then(reminder_time) {
            sqlx::query(
                "INSERT INTO notifications
                    (user_id, project_id, project_task_id, message, notification_type, actor_name, due_at)
                 VALUES (?, ?, ?, ?, ?, NULL, ?)",
            )
            .bind(assignee.unwrap_or(user.id))
            .bind(project_id)
            .bind(task_id)
            .bind(&message)
            .bind(TASK_DUE)
            .bind(due.naive_utc())
            .execute(&state.pool)
            .await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::error!(?error, task_id, "project task notification failed");
    }
}

/// Add a task
///
/// A task on the project for anyone in the company (or nobody). The person
/// gets a notification now and, with a due date, another when it is due (9 AM
/// store time on a date with no time).
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/tasks",
    operation_id = "create_project_task",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = TaskInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty name, bad date or priority, or someone outside the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<TaskInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name the task".into()));
    }
    let name = clip(name, 255);
    let deadline = input
        .deadline
        .as_deref()
        .map(parse_deadline)
        .transpose()?
        .flatten();
    let priority = priority(input.priority.as_deref())?;
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    if let Some(assignee) = input.assigned_user_id {
        require_colleague(&mut conn, viewer.company_id, assignee).await?;
    }
    let task = sqlx::query(
        "INSERT INTO project_activities
            (project_id, company_id, name, assigned_user_id, deadline, priority, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(viewer.company_id)
    .bind(&name)
    .bind(input.assigned_user_id)
    .bind(deadline.as_deref())
    .bind(priority)
    .bind(user.id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    let task_id = i32::try_from(task).map_err(|_| ApiError::Internal("task id".into()))?;
    drop(conn);
    notify(
        &state,
        &user,
        id,
        task_id,
        &name,
        input.assigned_user_id,
        deadline.as_deref(),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::from(task_id),
        }),
    ))
}

/// The task, if the caller may open its project: (creator, assignee).
async fn task_people(
    conn: &mut MySqlConnection,
    viewer: &Viewer,
    project_id: i32,
    task_id: i32,
) -> ApiResult<(i32, Option<i32>)> {
    visible_project(conn, viewer, project_id).await?;
    sqlx::query_as(
        "SELECT created_by, assigned_user_id FROM project_activities
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)
}

/// Complete or reopen a task
///
/// Its creator, the person it is for, or a project manager. A done task's
/// due reminder leaves the bell; reopening brings it back.
#[crate::openapi::path(
    patch,
    path = "/v1/projects/{id}/tasks/{task_id}",
    operation_id = "update_project_task",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("task_id" = i32, Path, description = "Task id"),
    ),
    request_body = TaskUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the task's creator, its person or a manager", body = ErrorBody),
        (status = 404, description = "No such task the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, task_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<TaskUpdate>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let (creator, assignee) = task_people(&mut conn, &viewer, id, task_id).await?;
    Viewer::require(viewer.permissions.manage || creator == user.id || assignee == Some(user.id))?;
    sqlx::query(
        "UPDATE project_activities SET completed_at = IF(?, UTC_TIMESTAMP(), NULL)
          WHERE id = ? AND company_id = ?",
    )
    .bind(input.completed)
    .bind(task_id)
    .bind(viewer.company_id)
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a task
///
/// Its creator or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/tasks/{task_id}",
    operation_id = "delete_project_task",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("task_id" = i32, Path, description = "Task id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the task's creator or a manager", body = ErrorBody),
        (status = 404, description = "No such task the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_task(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, task_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    let (creator, _) = task_people(&mut conn, &viewer, id, task_id).await?;
    Viewer::require(viewer.permissions.manage || creator == user.id)?;
    sqlx::query(
        "UPDATE project_activities SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND company_id = ?",
    )
    .bind(task_id)
    .bind(viewer.company_id)
    .execute(&mut *conn)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Add a note
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/notes",
    operation_id = "create_project_note",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = NoteInput,
    responses(
        (status = 201, description = "Created", body = CreatedId),
        (status = 400, description = "Empty note", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<NoteInput>,
) -> ApiResult<(StatusCode, Json<CreatedId>)> {
    let content = input.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("Write the note first".into()));
    }
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let note = sqlx::query(
        "INSERT INTO project_notes (project_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(viewer.company_id)
    .bind(clip(content, 20_000))
    .bind(user.id)
    .execute(&mut *conn)
    .await?
    .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(CreatedId {
            id: i64::try_from(note).unwrap_or_default(),
        }),
    ))
}

/// Delete a note
///
/// Its author or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/notes/{note_id}",
    operation_id = "delete_project_note",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("note_id" = i32, Path, description = "Note id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the note's author or a manager", body = ErrorBody),
        (status = 404, description = "No such note the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_note(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, note_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let author: i32 = sqlx::query_scalar(
        "SELECT created_by FROM project_notes
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(note_id)
    .bind(id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Viewer::require(viewer.permissions.manage || author == user.id)?;
    sqlx::query("UPDATE project_notes SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(note_id)
        .execute(&mut *conn)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Attach files
///
/// Files already uploaded with `POST /v1/uploads` (folder `deals`).
#[crate::openapi::path(
    post,
    path = "/v1/projects/{id}/files",
    operation_id = "attach_project_files",
    tag = "projects",
    params(("id" = i32, Path, description = "Project id")),
    request_body = NewProjectFiles,
    responses(
        (status = 204, description = "Attached"),
        (status = 400, description = "No files, or a URL that isn't the company's upload", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such project the caller may see", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn attach_files(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<NewProjectFiles>,
) -> ApiResult<StatusCode> {
    if body.files.is_empty() || body.files.len() > 20 {
        return Err(ApiError::BadRequest(
            "Attach 1 to 20 files at a time".into(),
        ));
    }
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    for file in &body.files {
        attach(
            &state.storage,
            &file.url,
            UploadFolder::Deals,
            viewer.company_id,
        )
        .await?;
    }
    let mut tx = state.pool.begin().await?;
    for file in &body.files {
        let name = file.name.trim();
        let content_type = file
            .content_type
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .unwrap_or("application/octet-stream");
        sqlx::query(
            "INSERT INTO project_files (project_id, company_id, name, url, content_type, created_by)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(viewer.company_id)
        .bind(clip(if name.is_empty() { "File" } else { name }, 255))
        .bind(&file.url)
        .bind(clip(content_type, 255))
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a file
///
/// Whoever attached it or a project manager.
#[crate::openapi::path(
    delete,
    path = "/v1/projects/{id}/files/{file_id}",
    operation_id = "delete_project_file",
    tag = "projects",
    params(
        ("id" = i32, Path, description = "Project id"),
        ("file_id" = i32, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not the file's uploader or a manager", body = ErrorBody),
        (status = 404, description = "No such file the caller may see", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_file(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, file_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    let viewer = Viewer::load(&state.pool, &user).await?;
    let mut conn = state.pool.acquire().await?;
    visible_project(&mut conn, &viewer, id).await?;
    let (creator, url): (i32, String) = sqlx::query_as(
        "SELECT created_by, url FROM project_files
          WHERE id = ? AND project_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(file_id)
    .bind(id)
    .bind(viewer.company_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ApiError::NotFound)?;
    Viewer::require(viewer.permissions.manage || creator == user.id)?;
    sqlx::query("UPDATE project_files SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(file_id)
        .execute(&mut *conn)
        .await?;
    drop(conn);
    remove(&state.storage, &url, viewer.company_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{parse_deadline, priority, reminder_time};

    #[test]
    fn deadlines() {
        assert_eq!(
            parse_deadline("2026-11-02").unwrap().as_deref(),
            Some("2026-11-02 00:00:00")
        );
        assert_eq!(
            parse_deadline("2026-11-02T14:30").unwrap().as_deref(),
            Some("2026-11-02 14:30:00")
        );
        assert_eq!(
            parse_deadline("2026-11-02 14:30:15").unwrap().as_deref(),
            Some("2026-11-02 14:30:15")
        );
        assert_eq!(parse_deadline("  ").unwrap(), None);
        assert!(parse_deadline("2026-02-30").is_err());
        assert!(parse_deadline("2026-11-02x14:30").is_err());
    }

    #[test]
    fn reminders_fire_at_the_time_or_9_am_store_time() {
        // November: Indianapolis is UTC-5.
        assert_eq!(
            reminder_time("2026-11-02 14:30:00").map(|at| at.to_rfc3339()),
            Some("2026-11-02T19:30:00+00:00".into())
        );
        assert_eq!(
            reminder_time("2026-11-02 00:00:00").map(|at| at.to_rfc3339()),
            Some("2026-11-02T14:00:00+00:00".into())
        );
    }

    #[test]
    fn priorities() {
        assert_eq!(priority(None).unwrap(), "medium");
        assert_eq!(priority(Some("high")).unwrap(), "high");
        assert!(priority(Some("urgent")).is_err());
    }
}
