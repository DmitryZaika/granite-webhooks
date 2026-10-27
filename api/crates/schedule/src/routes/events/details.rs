//! An event's comments, files, attachments and history; event search.
//!
//! Comment files live in `event_images`. Remix: `api.event-details.$eventId`,
//! `api.event-attachments.$eventId`, `api.event-images.$eventId` and
//! `api.events.search`.

use super::history::{History, record};
use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::routes::uploads::{self, schemas::UploadFolder};
use crate::sql::escape_like;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

const DOCUMENT_EXTENSIONS: [&str; 7] = ["pdf", "doc", "docx", "xls", "xlsx", "csv", "txt"];
const IMAGE_EXTENSIONS: [&str; 7] = ["jpg", "jpeg", "png", "gif", "webp", "heic", "heif"];

/// A comment on an event.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct EventComment {
    /// The comment.
    pub id: i32,
    /// Its event.
    pub event_id: i32,
    /// The text.
    pub content: String,
    /// When it was written.
    pub created_at: Option<DateTime<Utc>>,
    /// Author's name.
    pub created_by: Option<String>,
}

/// A file posted in the event's comments.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventPhoto {
    /// The file.
    pub id: i32,
    /// Its event.
    pub event_id: i32,
    /// Where it is stored.
    pub url: String,
    /// Last part of the URL.
    pub name: String,
    /// When it was posted.
    pub created_at: Option<DateTime<Utc>>,
    /// Poster's name.
    pub created_by: Option<String>,
    /// Always `photo`.
    pub source: &'static str,
}

/// A document or image attached to the event.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct EventAttachment {
    /// The attachment.
    pub id: i32,
    /// Its event.
    pub event_id: i32,
    /// Where it is stored.
    pub url: String,
    /// File name.
    pub name: String,
    /// `document` or `image`.
    pub kind: String,
    /// When it was attached.
    pub created_at: Option<DateTime<Utc>>,
    /// Who attached it.
    pub created_by: Option<String>,
}

/// A line of the event's history.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct EventHistoryItem {
    /// The line.
    pub id: i32,
    /// Its event.
    pub event_id: i32,
    /// e.g. `created`, `date_updated`.
    pub action: String,
    /// Shown text.
    pub message: String,
    /// When.
    pub created_at: Option<DateTime<Utc>>,
    /// Who.
    pub created_by: Option<String>,
}

/// A user on the event.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventPerson {
    /// The user.
    pub id: i32,
    /// Their name.
    pub name: String,
}

/// `GET /v1/events/{id}/details`.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventDetails {
    /// Oldest first.
    pub comments: Vec<EventComment>,
    /// Files posted in the comments, oldest first.
    pub photos: Vec<EventPhoto>,
    /// Attachments, oldest first.
    pub attachments: Vec<EventAttachment>,
    /// History, oldest first.
    pub history: Vec<EventHistoryItem>,
    /// Who made the event.
    pub creator: EventPerson,
    /// Who it is assigned to.
    pub assignee: Option<EventPerson>,
    /// The caller's name (comments are theirs when the author matches).
    pub viewer_name: Option<String>,
}

/// id, url, created at, created by.
type ImageRow = (i32, String, Option<DateTime<Utc>>, Option<String>);

#[derive(sqlx::FromRow)]
struct EventPeople {
    created_user_id: i32,
    creator_name: Option<String>,
    assigned_user_id: Option<i32>,
    assignee_name: Option<String>,
}

async fn people(pool: &MySqlPool, company_id: i32, event_id: i32) -> ApiResult<EventPeople> {
    let found: Option<EventPeople> = sqlx::query_as(
        "SELECT e.created_user_id, u.name AS creator_name, e.assigned_user_id, au.name AS assignee_name
           FROM events e JOIN users u ON e.created_user_id = u.id
           LEFT JOIN users au ON e.assigned_user_id = au.id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.ok_or(ApiError::NotFound)
}

fn last_segment(url: &str) -> &str {
    let clean = url.split('?').next().unwrap_or(url);
    clean.rsplit('/').next().unwrap_or(clean)
}

/// Get an event's comments, files and history
///
/// Everything the event panel shows besides the event itself, oldest first.
#[crate::openapi::path(
    get,
    path = "/v1/events/{id}/details",
    operation_id = "get_event_details",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    responses(
        (status = 200, description = "Details", body = EventDetails),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn details(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<EventDetails>> {
    let pool = &state.pool;
    let event = people(pool, user.company_id, id).await?;
    let comments = sqlx::query_as(
        "SELECT id, event_id, content, created_at, created_by FROM event_comments
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let images: Vec<ImageRow> = sqlx::query_as(
        "SELECT id, url, created_at, created_by FROM event_images
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let attachments = sqlx::query_as(
        "SELECT id, event_id, url, name, kind, created_at, created_by FROM event_attachments
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL
            AND kind IN ('document', 'image') ORDER BY created_at, id",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let history = sqlx::query_as(
        "SELECT id, event_id, action, message, created_at, created_by FROM event_history
          WHERE event_id = ? AND company_id = ? ORDER BY created_at, id",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(pool)
    .await?;
    let photos = images
        .into_iter()
        .map(|(photo_id, url, created_at, created_by)| EventPhoto {
            id: photo_id,
            event_id: id,
            name: match last_segment(&url) {
                "" => "photo".to_string(),
                name => name.to_string(),
            },
            url,
            created_at,
            created_by,
            source: "photo",
        })
        .collect();
    let assignee = match (event.assigned_user_id, event.assignee_name) {
        (Some(assignee_id), Some(name)) => Some(EventPerson {
            id: assignee_id,
            name,
        }),
        _ => None,
    };
    Ok(Json(EventDetails {
        comments,
        photos,
        attachments,
        history,
        creator: EventPerson {
            id: event.created_user_id,
            name: event.creator_name.unwrap_or_default(),
        },
        assignee,
        viewer_name: user.name,
    }))
}

/// Body of the comment routes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CommentInput {
    /// 1 to 2000 characters (trimmed).
    pub content: String,
}

fn comment_text(input: &CommentInput) -> ApiResult<&str> {
    let content = input.content.trim();
    if content.is_empty() {
        return Err(ApiError::BadRequest("Comment content is required".into()));
    }
    if content.chars().count() > 2000 {
        return Err(ApiError::BadRequest(
            "Comment must be 2000 characters or less".into(),
        ));
    }
    Ok(content)
}

/// Rows of the caller's own: `created_by` matches their name, ignoring
/// case and spaces.
const OWN: &str = "LOWER(TRIM(COALESCE(created_by, ''))) = LOWER(TRIM(?))";

fn viewer(user: &SessionUser) -> &str {
    user.name.as_deref().unwrap_or("")
}

/// Comment on an event
///
/// Adds a comment signed with the caller's name.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/comments",
    operation_id = "add_event_comment",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = CommentInput,
    responses(
        (status = 201, description = "Added"),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_comment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<CommentInput>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    let content = comment_text(&input)?;
    sqlx::query(
        "INSERT INTO event_comments (event_id, company_id, content, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user.company_id)
    .bind(content)
    .bind(&user.name)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::CREATED)
}

/// Edit my comment
///
/// Only the author can edit a comment.
#[crate::openapi::path(
    put,
    path = "/v1/events/{id}/comments/{comment_id}",
    operation_id = "edit_event_comment",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("comment_id" = i32, Path, description = "Comment id"),
    ),
    request_body = CommentInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's comment on this event", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn edit_comment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, comment_id)): Path<(i32, i32)>,
    JsonBody(input): JsonBody<CommentInput>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    let content = comment_text(&input)?;
    let own: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM event_comments
          WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL AND {OWN}"
    )))
    .bind(comment_id)
    .bind(id)
    .bind(user.company_id)
    .bind(viewer(&user))
    .fetch_optional(&state.pool)
    .await?;
    own.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE event_comments SET content = ? WHERE id = ?")
        .bind(content)
        .bind(comment_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete my comment
///
/// Only the author can delete a comment.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/comments/{comment_id}",
    operation_id = "delete_event_comment",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("comment_id" = i32, Path, description = "Comment id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's comment on this event", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_comment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, comment_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    let deleted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE event_comments SET deleted_at = NOW()
          WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL AND {OWN}"
    )))
    .bind(comment_id)
    .bind(id)
    .bind(user.company_id)
    .bind(viewer(&user))
    .execute(&state.pool)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `POST /v1/events/{id}/photos`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PhotosInput {
    /// Files uploaded to the `events` folder (`POST /v1/uploads`).
    pub urls: Vec<String>,
}

/// Post files in an event's comments
///
/// Files already uploaded with `POST /v1/uploads` (folder `events`).
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/photos",
    operation_id = "add_event_photos",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = PhotosInput,
    responses(
        (status = 201, description = "Posted"),
        (status = 400, description = "No files, or a URL that was not uploaded", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_photos(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<PhotosInput>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    if input.urls.is_empty() {
        return Err(ApiError::BadRequest("No files".into()));
    }
    for url in &input.urls {
        uploads::attach(&state.storage, url, UploadFolder::Events, user.company_id).await?;
    }
    for url in &input.urls {
        sqlx::query(
            "INSERT INTO event_images (event_id, company_id, url, created_by) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(user.company_id)
        .bind(url)
        .bind(&user.name)
        .execute(&state.pool)
        .await?;
    }
    Ok(StatusCode::CREATED)
}

/// Deletes a stored file once no live event points at it.
async fn remove_if_unused(state: &AppState, company_id: i32, url: &str) -> Result<(), sqlx::Error> {
    let used: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM event_attachments WHERE company_id = ? AND url = ? AND deleted_at IS NULL)
              + (SELECT COUNT(*) FROM event_images WHERE company_id = ? AND url = ? AND deleted_at IS NULL)",
    )
    .bind(company_id)
    .bind(url)
    .bind(company_id)
    .bind(url)
    .fetch_one(&state.pool)
    .await?;
    if used == 0 {
        uploads::remove(&state.storage, url, company_id).await;
    }
    Ok(())
}

/// Delete my comment file
///
/// Only the poster can delete it; the stored file goes once nothing uses it.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/photos/{photo_id}",
    operation_id = "delete_event_photo",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("photo_id" = i32, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not the caller's file on this event", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_photo(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, photo_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    let url: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT url FROM event_images
          WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL AND {OWN}"
    )))
    .bind(photo_id)
    .bind(id)
    .bind(user.company_id)
    .bind(viewer(&user))
    .fetch_optional(&state.pool)
    .await?;
    let url = url.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE event_images SET deleted_at = NOW() WHERE id = ?")
        .bind(photo_id)
        .execute(&state.pool)
        .await?;
    remove_if_unused(&state, user.company_id, &url).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A file to attach.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachmentFile {
    /// Uploaded to the `events` folder (`POST /v1/uploads`).
    pub url: String,
    /// File name to show; defaults to the URL's last part.
    #[serde(default)]
    pub name: Option<String>,
}

/// Body of `POST /v1/events/{id}/attachments`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachmentsInput {
    /// Documents (pdf, Word, Excel, csv, txt) or images.
    pub files: Vec<AttachmentFile>,
}

fn attachment_kind(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        Some("image")
    } else if DOCUMENT_EXTENSIONS.contains(&extension.as_str()) {
        Some("document")
    } else {
        None
    }
}

/// Attach files to an event
///
/// Files already uploaded with `POST /v1/uploads` (folder `events`); the
/// kind comes from the extension.
#[crate::openapi::path(
    post,
    path = "/v1/events/{id}/attachments",
    operation_id = "add_event_attachments",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = AttachmentsInput,
    responses(
        (status = 201, description = "Attached"),
        (status = 400, description = "No files, an unsupported type, or a URL that was not uploaded", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_attachments(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<AttachmentsInput>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    if input.files.is_empty() {
        return Err(ApiError::BadRequest("No files".into()));
    }
    let mut rows = Vec::new();
    for file in &input.files {
        let kind = attachment_kind(last_segment(&file.url))
            .ok_or_else(|| ApiError::BadRequest("Unsupported attachment type".into()))?;
        let name = file
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map_or_else(|| last_segment(&file.url).to_string(), str::to_string);
        uploads::attach(
            &state.storage,
            &file.url,
            UploadFolder::Events,
            user.company_id,
        )
        .await?;
        rows.push((
            file.url.as_str(),
            name.chars().take(255).collect::<String>(),
            kind,
        ));
    }
    for (url, name, kind) in rows {
        sqlx::query(
            "INSERT INTO event_attachments (event_id, company_id, url, name, kind, created_by)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(user.company_id)
        .bind(url)
        .bind(name)
        .bind(kind)
        .bind(&user.name)
        .execute(&state.pool)
        .await?;
    }
    record(
        &state.pool,
        &History::new(
            id,
            user.company_id,
            "attachment_added",
            user.name.as_deref(),
        ),
    )
    .await;
    Ok(StatusCode::CREATED)
}

/// Remove an attachment
///
/// Any employee may remove one; the stored file goes once nothing uses it.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}/attachments/{attachment_id}",
    operation_id = "delete_event_attachment",
    tag = "schedule",
    params(
        ("id" = i32, Path, description = "Event id"),
        ("attachment_id" = i32, Path, description = "Attachment id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such attachment on this event", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_attachment(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, attachment_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    people(&state.pool, user.company_id, id).await?;
    let url: Option<String> = sqlx::query_scalar(
        "SELECT url FROM event_attachments
          WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(attachment_id)
    .bind(id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    let url = url.ok_or(ApiError::NotFound)?;
    sqlx::query("UPDATE event_attachments SET deleted_at = NOW() WHERE id = ?")
        .bind(attachment_id)
        .execute(&state.pool)
        .await?;
    remove_if_unused(&state, user.company_id, &url).await?;
    record(
        &state.pool,
        &History::new(
            id,
            user.company_id,
            "attachment_removed",
            user.name.as_deref(),
        ),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Query of `GET /v1/events/search`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventSearchQuery {
    /// Part of the title, customer name, location, description or notes.
    #[serde(default)]
    pub q: String,
    /// At most this many, 1 to 50 (default 25).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct FoundRow {
    id: i32,
    title: String,
    description: Option<String>,
    location: Option<String>,
    notes: Option<String>,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    all_day: bool,
    color: Option<String>,
    status: Option<String>,
    calendar_id: Option<i32>,
    rrule: Option<String>,
    recurrence_until: Option<NaiveDateTime>,
    created_user_id: i32,
    assigned_user_id: Option<i32>,
    sale_id: Option<i32>,
    customer_id: Option<i32>,
    customer_name: Option<String>,
}

/// An event found by the search.
#[derive(Debug, Serialize, ToSchema)]
pub struct FoundEvent {
    /// The event.
    pub id: i32,
    /// The event's name.
    pub title: String,
    /// Longer text.
    pub description: Option<String>,
    /// Address.
    pub location: Option<String>,
    /// Internal notes.
    pub notes: Option<String>,
    /// Start (UTC).
    pub start_date: DateTime<Utc>,
    /// End (UTC).
    pub end_date: DateTime<Utc>,
    /// All-day event.
    pub all_day: bool,
    /// Label color.
    pub color: Option<String>,
    /// e.g. `scheduled`.
    pub status: Option<String>,
    /// Its calendar.
    pub calendar_id: Option<i32>,
    /// Repeat rule.
    pub rrule: Option<String>,
    /// Last repeat (UTC).
    pub recurrence_until: Option<DateTime<Utc>>,
    /// Who made it.
    pub created_user_id: i32,
    /// Who it is assigned to.
    pub assigned_user_id: Option<i32>,
    /// Its sale.
    pub sale_id: Option<i32>,
    /// The customer it is for.
    pub customer_id: Option<i32>,
    /// That customer's name.
    pub customer_name: Option<String>,
}

/// Search events
///
/// The company's events whose title, customer's name, location, description
/// or notes contain `q`, newest first.
#[crate::openapi::path(
    get,
    path = "/v1/events/search",
    operation_id = "search_events",
    tag = "schedule",
    params(EventSearchQuery),
    responses(
        (status = 200, description = "Events", body = [FoundEvent]),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn search(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<EventSearchQuery>,
) -> ApiResult<Json<Vec<FoundEvent>>> {
    let q = query.q.trim();
    if q.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let limit = query.limit.unwrap_or(25).clamp(1, 50);
    let like = format!("%{}%", escape_like(q));
    let own = crate::routes::schedule::own_events_only(&state.pool, &user).await?;
    let rows: Vec<FoundRow> = sqlx::query_as(
        "SELECT e.id, e.title, e.description, e.location, e.notes, e.start_date, e.end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.color, e.status, e.calendar_id, e.rrule,
                e.recurrence_until, e.created_user_id, e.assigned_user_id, e.sale_id,
                e.customer_id, c.name AS customer_name
           FROM events e
           JOIN users u ON e.created_user_id = u.id
           LEFT JOIN customers c ON c.id = e.customer_id
                AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND e.deleted_date IS NULL
            AND (e.title LIKE ? OR c.name LIKE ? OR e.location LIKE ? OR e.description LIKE ? OR e.notes LIKE ?)
            AND (? IS NULL OR e.created_user_id = ? OR e.assigned_user_id = ?)
          ORDER BY e.start_date DESC
          LIMIT ?",
    )
    .bind(user.company_id)
    .bind(&like)
    .bind(&like)
    .bind(&like)
    .bind(&like)
    .bind(&like)
    .bind(own)
    .bind(own)
    .bind(own)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| FoundEvent {
                id: row.id,
                title: row.title,
                description: row.description,
                location: row.location,
                notes: row.notes,
                start_date: row.start_date.and_utc(),
                end_date: row.end_date.and_utc(),
                all_day: row.all_day,
                color: row.color,
                status: row.status,
                calendar_id: row.calendar_id,
                rrule: row.rrule,
                recurrence_until: row.recurrence_until.map(|at| at.and_utc()),
                created_user_id: row.created_user_id,
                assigned_user_id: row.assigned_user_id,
                sale_id: row.sale_id,
                customer_id: row.customer_id,
                customer_name: row.customer_name,
            })
            .collect(),
    ))
}
