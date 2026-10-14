//! The company's document library, as admins keep it.
//!
//! - `POST   /v1/documents`      → add a document (uploaded to `documents/`)
//! - `PUT    /v1/documents/{id}` → rename it, or replace its file
//! - `DELETE /v1/documents/{id}` → remove it from the library
//! - `GET    /v1/documents/{id}/preview` → a picture of a PDF's first page
//!
//! `GET /v1/documents` (the list) is in [`super::attachable`]. Replaces the
//! Remix `admin.documents.*` actions and `api.documents.$documentId.preview`.

use super::attachable::LibraryFile;
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::storage::Storage;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(update, delete))
        .routes(routes!(preview))
}

/// Body of `POST /v1/documents`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewDocument {
    /// Name in the library (1–255 characters).
    pub name: String,
    /// URL of the file uploaded to the `documents` folder.
    pub url: String,
}

/// Body of `PUT /v1/documents/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DocumentUpdate {
    /// New name (1–255 characters).
    pub name: String,
    /// A newly uploaded file replacing the old one; leave out to keep it.
    #[serde(default)]
    pub url: Option<String>,
}

fn checked_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Name is required (at most 255 characters)".into(),
        ));
    }
    Ok(name.to_string())
}

fn checked_url(storage: &Storage, url: &str) -> ApiResult<()> {
    if storage
        .key_for_url(url)
        .is_some_and(|key| key.starts_with("documents/"))
    {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!(
            "not an uploaded document: {url}"
        )))
    }
}

/// The page image the CRM renders for a PDF (`withPdfPreviewSuffix`).
fn preview_url(url: &str) -> String {
    let stem = url
        .rfind('.')
        .filter(|dot| !url[*dot..].contains('/'))
        .map_or(url, |dot| &url[..dot]);
    format!("{stem}-preview.webp")
}

async fn find(state: &AppState, company_id: i32, id: i32) -> ApiResult<LibraryFile> {
    sqlx::query_as("SELECT id, name, url FROM documents WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(ApiError::NotFound)
}

/// Add a document
///
/// Puts a file uploaded to the `documents` folder (`POST /v1/uploads`) in the
/// company's library under `name`.
#[utoipa::path(
    post,
    path = "/v1/documents",
    operation_id = "create_document",
    tag = "catalog",
    request_body = NewDocument,
    responses(
        (status = 201, description = "Added", body = LibraryFile),
        (status = 400, description = "No name, or not an uploaded document", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<NewDocument>,
) -> ApiResult<(StatusCode, Json<LibraryFile>)> {
    let name = checked_name(&input.name)?;
    checked_url(&state.storage, &input.url)?;
    let id = sqlx::query("INSERT INTO documents (name, url, company_id) VALUES (?, ?, ?)")
        .bind(&name)
        .bind(&input.url)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .last_insert_id();
    Ok((
        StatusCode::CREATED,
        Json(LibraryFile {
            id: i32::try_from(id).unwrap_or(i32::MAX),
            name: Some(name),
            url: Some(input.url),
        }),
    ))
}

/// Edit a document
///
/// Renames it; with `url`, its file is replaced by a new upload and the old
/// file (and its page preview) is deleted from storage.
#[utoipa::path(
    put,
    path = "/v1/documents/{id}",
    operation_id = "update_document",
    tag = "catalog",
    params(("id" = i32, Path, description = "Document id")),
    request_body = DocumentUpdate,
    responses(
        (status = 200, description = "Saved", body = LibraryFile),
        (status = 400, description = "No name, or not an uploaded document", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such document in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<DocumentUpdate>,
) -> ApiResult<Json<LibraryFile>> {
    let name = checked_name(&input.name)?;
    let old = find(&state, user.company_id, id).await?;
    let new_url = input.url.filter(|url| Some(url) != old.url.as_ref());
    if let Some(url) = &new_url {
        checked_url(&state.storage, url)?;
    }
    sqlx::query(
        "UPDATE documents SET name = ?, url = COALESCE(?, url) WHERE id = ? AND company_id = ?",
    )
    .bind(&name)
    .bind(&new_url)
    .bind(id)
    .bind(user.company_id)
    .execute(&state.pool)
    .await?;
    if let (Some(_), Some(old_url)) = (&new_url, &old.url) {
        super::uploads::remove(&state.storage, old_url).await;
        super::uploads::remove(&state.storage, &preview_url(old_url)).await;
    }
    Ok(Json(find(&state, user.company_id, id).await?))
}

/// Delete a document
///
/// Takes it out of the library. The file stays in storage: emails may
/// already carry it.
#[utoipa::path(
    delete,
    path = "/v1/documents/{id}",
    operation_id = "delete_document",
    tag = "catalog",
    params(("id" = i32, Path, description = "Document id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such document in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let deleted = sqlx::query("DELETE FROM documents WHERE id = ? AND company_id = ?")
        .bind(id)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::preview_url;

    #[test]
    fn previews_sit_next_to_the_pdf() {
        assert_eq!(
            preview_url("https://b.s3.amazonaws.com/documents/a.b.pdf"),
            "https://b.s3.amazonaws.com/documents/a.b-preview.webp"
        );
        assert_eq!(
            preview_url("https://b.test/documents/plain"),
            "https://b.test/documents/plain-preview.webp"
        );
    }
}

/// Biggest PDF read to draw its first page.
const MAX_PREVIEW_SOURCE_BYTES: usize = 40 << 20;
/// Render time allowed before giving up (the CRM's 20 s).
const PREVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// After a failed render, how long to answer "unavailable" without retrying.
const PREVIEW_RETRY_AFTER: std::time::Duration = std::time::Duration::from_mins(10);

static FAILED_PREVIEWS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = std::sync::LazyLock::new(Default::default);

/// The stored first-page picture of a PDF in the bucket, drawn on first
/// use; `None` when it cannot be made.
pub async fn ensure_pdf_preview(storage: &Storage, pdf_url: &str) -> ApiResult<Option<String>> {
    let Some(key) = storage.key_for_url(pdf_url) else {
        return Ok(None);
    };
    let preview_key = storage
        .key_for_url(&preview_url(pdf_url))
        .ok_or(ApiError::NotFound)?;
    if storage.exists(&preview_key).await? {
        return Ok(Some(storage.public_url(&preview_key)?));
    }
    {
        let mut failed = FAILED_PREVIEWS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failed
            .get(&key)
            .is_some_and(|at| at.elapsed() < PREVIEW_RETRY_AFTER)
        {
            return Ok(None);
        }
        failed.remove(&key);
    }
    let rendered = match storage.get(&key).await? {
        Some(object) if object.bytes.len() <= MAX_PREVIEW_SOURCE_BYTES => {
            let render = tokio::task::spawn_blocking(move || {
                crate::pdf_preview::first_page_webp(object.bytes)
            });
            tokio::time::timeout(PREVIEW_TIMEOUT, render)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
        }
        _ => None,
    };
    let Some(webp) = rendered else {
        FAILED_PREVIEWS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, std::time::Instant::now());
        return Ok(None);
    };
    storage.put(&preview_key, webp, "image/webp").await?;
    Ok(Some(storage.public_url(&preview_key)?))
}

/// Show a PDF's first page
///
/// Redirects to a picture (WebP, 400 px wide) of the first page of a PDF in
/// the company's document library, drawing it on first use. For an `<img>`.
#[utoipa::path(
    get,
    path = "/v1/documents/{id}/preview",
    operation_id = "preview_document",
    tag = "catalog",
    params(("id" = i32, Path, description = "Document id")),
    responses(
        (status = 302, description = "Redirect to the page picture"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such PDF in the library, or no picture could be drawn", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn preview(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    let url: Option<String> =
        sqlx::query_scalar("SELECT url FROM documents WHERE id = ? AND company_id = ?")
            .bind(id)
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();
    let Some(url) = url.filter(|url| url.to_lowercase().ends_with(".pdf")) else {
        return Err(ApiError::NotFound);
    };
    let Some(preview) = ensure_pdf_preview(&state.storage, &url).await? else {
        return Err(ApiError::NotFound);
    };
    Ok((
        StatusCode::FOUND,
        [
            (axum::http::header::LOCATION, preview),
            (
                axum::http::header::CACHE_CONTROL,
                "private, max-age=86400".to_string(),
            ),
        ],
    )
        .into_response())
}
