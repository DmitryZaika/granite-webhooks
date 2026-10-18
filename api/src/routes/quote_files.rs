//! Files attached to a quote (`quote_files`).
//!
//! - `GET /v1/quotes/{id}/files` → the quote's files, newest first
//! - `POST /v1/quotes/{id}/files` → attach files uploaded to `quote-files`
//! - `DELETE /v1/quotes/{id}/files/{file_id}` → remove one, and its stored file

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::deals::files::document_content_type;
use crate::routes::uploads::{attach, remove, schemas::UploadFolder};
use crate::serde_helpers::js_date;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list, attach_files))
        .routes(routes!(delete_file))
}

/// A file attached to a quote.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct QuoteFile {
    /// Row id.
    pub id: i32,
    /// URL the browser can open (presigned for private files).
    pub url: String,
    /// File name shown in the list.
    pub name: String,
    /// Who attached it.
    pub created_by: Option<String>,
    /// When it was attached.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// One uploaded file to attach.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewQuoteFile {
    /// Public URL from `POST /v1/uploads` (folder `quote-files`).
    pub url: String,
    /// Original file name.
    pub name: String,
}

/// Body of `POST /v1/quotes/{id}/files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewQuoteFiles {
    /// Files already uploaded with `POST /v1/uploads`.
    pub files: Vec<NewQuoteFile>,
}

async fn require_quote(state: &AppState, company_id: i32, id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM quotes WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.map(|_| ()).ok_or(ApiError::NotFound)
}

/// List a quote's files
///
/// Newest first. Private files come with a link valid for an hour.
#[utoipa::path(
    get,
    path = "/v1/quotes/{id}/files",
    operation_id = "list_quote_files",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    responses(
        (status = 200, description = "Files", body = Vec<QuoteFile>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live quote in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<QuoteFile>>> {
    require_quote(&state, user.company_id, id).await?;
    let files: Vec<QuoteFile> = sqlx::query_as(
        "SELECT id, url, name, created_by, created_at FROM quote_files
          WHERE quote_id = ? ORDER BY created_at DESC, id DESC",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let mut signed = Vec::with_capacity(files.len());
    for mut file in files {
        let content_type = document_content_type(&file.url);
        file.url = state.storage.readable_url(&file.url, content_type).await;
        signed.push(file);
    }
    Ok(Json(signed))
}

/// Attach uploaded files to a quote
///
/// Each URL must be a file uploaded to the `quote-files` folder.
#[utoipa::path(
    post,
    path = "/v1/quotes/{id}/files",
    operation_id = "attach_quote_files",
    tag = "quotes",
    params(("id" = i32, Path, description = "Quote id")),
    request_body = NewQuoteFiles,
    responses(
        (status = 201, description = "Attached"),
        (status = 400, description = "No files, a blank name, or a URL that is not an uploaded quote file", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live quote in the company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured on this server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn attach_files(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<NewQuoteFiles>,
) -> ApiResult<StatusCode> {
    if body.files.is_empty() {
        return Err(ApiError::BadRequest("no files".into()));
    }
    if body.files.iter().any(|f| f.name.trim().is_empty()) {
        return Err(ApiError::BadRequest("a file has no name".into()));
    }
    require_quote(&state, user.company_id, id).await?;
    for file in &body.files {
        attach(&state.storage, &file.url, UploadFolder::QuoteFiles).await?;
    }
    let created_by = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(100).collect::<String>());
    let mut tx = state.pool.begin().await?;
    for file in &body.files {
        sqlx::query(
            "INSERT INTO quote_files (quote_id, url, name, created_by) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(&file.url)
        .bind(file.name.trim().chars().take(255).collect::<String>())
        .bind(&created_by)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::CREATED)
}

/// Delete a quote file
///
/// Removes the row and the stored file.
#[utoipa::path(
    delete,
    path = "/v1/quotes/{id}/files/{file_id}",
    operation_id = "delete_quote_file",
    tag = "quotes",
    params(
        ("id" = i32, Path, description = "Quote id"),
        ("file_id" = i32, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such file on a live quote of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_file(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, file_id)): Path<(i32, i32)>,
) -> ApiResult<StatusCode> {
    require_quote(&state, user.company_id, id).await?;
    let url: String =
        sqlx::query_scalar("SELECT url FROM quote_files WHERE id = ? AND quote_id = ?")
            .bind(file_id)
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or(ApiError::NotFound)?;
    sqlx::query("DELETE FROM quote_files WHERE id = ?")
        .bind(file_id)
        .execute(&state.pool)
        .await?;
    remove(&state.storage, &url).await;
    Ok(StatusCode::NO_CONTENT)
}
