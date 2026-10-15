//! Photos and documents attached to a deal (`deals_images`, `deals_documents`).
//!
//! Older uploads put documents in `deals_images` too; a file counts as a
//! document by its extension wherever it is stored.

use super::queries::deal_in_company;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
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

const DOCUMENT_EXTENSIONS: &[&str] = &["pdf", "doc", "docx", "xls", "xlsx", "csv", "txt"];

fn extension(name: &str) -> Option<String> {
    let clean = name.split('?').next().unwrap_or_default().to_lowercase();
    clean.rsplit_once('.').map(|(_, ext)| ext.to_string())
}

fn is_document(name: &str) -> bool {
    extension(name).is_some_and(|ext| DOCUMENT_EXTENSIONS.contains(&ext.as_str()))
}

fn document_content_type(url: &str) -> Option<&'static str> {
    Some(match extension(url)?.as_str() {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => return None,
    })
}

/// A photo of a deal.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DealImage {
    /// Row id (in `images`).
    pub id: i32,
    /// URL the browser can show (presigned for private files).
    pub url: String,
    /// When it was attached.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
}

/// Where a file row lives; pass it back when deleting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileSource {
    /// A photo row (also older documents).
    Images,
    /// A document row.
    Documents,
}

/// A document of a deal.
#[derive(Debug, Clone, Serialize, FromRow, ToSchema)]
pub struct DealDocument {
    /// Row id within `source`.
    pub id: i32,
    /// URL the browser can open (presigned for private files).
    pub url: String,
    /// When it was attached.
    #[serde(serialize_with = "js_date")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<DateTime<Utc>>,
    /// Where the row lives.
    #[sqlx(skip)]
    pub source: Option<FileSource>,
    #[serde(skip)]
    pub source_name: String,
    /// Display name; null for older documents (show the file name).
    pub name: Option<String>,
}

/// Photos and documents of a deal, newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DealFiles {
    /// Photos.
    pub images: Vec<DealImage>,
    /// Documents.
    pub documents: Vec<DealDocument>,
}

/// One uploaded file to attach.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewFile {
    /// Public URL from `POST /v1/uploads` (folder `deals`).
    pub url: String,
    /// Original file name; documents keep it as their name.
    pub name: Option<String>,
}

/// Body of `POST /v1/deals/{id}/files`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewFiles {
    /// Files already uploaded with `POST /v1/uploads`.
    pub files: Vec<NewFile>,
}

/// How many of each kind were attached.
#[derive(Debug, Serialize, ToSchema)]
pub struct AttachedCounts {
    /// Photos attached.
    pub images: usize,
    /// Documents attached.
    pub documents: usize,
}

/// Body of `PATCH /v1/deals/{id}/documents/{document_id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DocumentRename {
    /// New display name, 1 to 255 characters.
    pub name: String,
}

const DOCUMENT_SQL: &str =
    "(LOWER(SUBSTRING_INDEX(image_url, '?', 1)) REGEXP '\\\\.(pdf|docx?|xlsx?|csv|txt)$')";

async fn require_deal(state: &AppState, company_id: i32, deal_id: i64) -> ApiResult<()> {
    if deal_in_company(&state.pool, company_id, deal_id).await? {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

/// List a deal's photos and documents
///
/// Newest first. Private files come with a link valid for an hour.
#[utoipa::path(
    get,
    path = "/v1/deals/{id}/files",
    operation_id = "list_deal_files",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    responses(
        (status = 200, description = "Photos and documents", body = DealFiles),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<DealFiles>> {
    require_deal(&state, user.company_id, id).await?;
    let images: Vec<DealImage> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, image_url AS url, created_at FROM deals_images
         WHERE deal_id = ? AND NOT {DOCUMENT_SQL} ORDER BY created_at DESC, id DESC"
    )))
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    let documents: Vec<DealDocument> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, image_url AS url, created_at, 'documents' AS source_name, name
           FROM deals_documents WHERE deal_id = ?
         UNION ALL
         SELECT id, image_url AS url, created_at, 'images' AS source_name, NULL AS name
           FROM deals_images WHERE deal_id = ? AND {DOCUMENT_SQL}
         ORDER BY created_at DESC, id DESC"
    )))
    .bind(id)
    .bind(id)
    .fetch_all(&state.pool)
    .await?;

    let mut signed_images = Vec::with_capacity(images.len());
    for mut image in images {
        image.url = state.storage.readable_url(&image.url, None).await;
        signed_images.push(image);
    }
    let mut signed_documents = Vec::with_capacity(documents.len());
    for mut document in documents {
        let content_type = document_content_type(&document.url);
        document.url = state
            .storage
            .readable_url(&document.url, content_type)
            .await;
        document.source = Some(if document.source_name == "documents" {
            FileSource::Documents
        } else {
            FileSource::Images
        });
        signed_documents.push(document);
    }
    Ok(Json(DealFiles {
        images: signed_images,
        documents: signed_documents,
    }))
}

/// Attach uploaded files to a deal
///
/// Each URL must be a file uploaded to the `deals` folder. PDFs, Word, Excel,
/// CSV and text files become documents (named after `name`, without the
/// upload id prefix), anything else a photo.
#[utoipa::path(
    post,
    path = "/v1/deals/{id}/files",
    operation_id = "attach_deal_files",
    tag = "deals",
    params(("id" = i64, Path, description = "Deal id")),
    request_body = NewFiles,
    responses(
        (status = 201, description = "Attached", body = AttachedCounts),
        (status = 400, description = "No files, or a URL that is not an uploaded deal file", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such live deal in the company", body = ErrorBody),
        (status = 500, description = "Database or storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured on this server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn attach_files(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i64>,
    JsonBody(body): JsonBody<NewFiles>,
) -> ApiResult<(StatusCode, Json<AttachedCounts>)> {
    if body.files.is_empty() {
        return Err(ApiError::BadRequest("no files".into()));
    }
    require_deal(&state, user.company_id, id).await?;
    for file in &body.files {
        attach(&state.storage, &file.url, UploadFolder::Deals).await?;
    }
    let created_by = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(100).collect::<String>());
    let mut counts = AttachedCounts {
        images: 0,
        documents: 0,
    };
    let mut tx = state.pool.begin().await?;
    for file in &body.files {
        let name = file.name.as_deref().unwrap_or_default();
        if is_document(name) || is_document(&file.url) {
            let display = display_name(name);
            sqlx::query(
                "INSERT INTO deals_documents (deal_id, image_url, created_by, name) VALUES (?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&file.url)
            .bind(&created_by)
            .bind(display)
            .execute(&mut *tx)
            .await?;
            counts.documents += 1;
        } else {
            sqlx::query(
                "INSERT INTO deals_images (deal_id, image_url, created_by) VALUES (?, ?, ?)",
            )
            .bind(id)
            .bind(&file.url)
            .bind(&created_by)
            .execute(&mut *tx)
            .await?;
            counts.images += 1;
        }
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(counts)))
}

/// The file name without the `uuid-` prefix uploads used to get; None when blank.
fn display_name(name: &str) -> Option<String> {
    let name = name.trim();
    let stripped = match name.get(..37) {
        Some(prefix) if prefix.ends_with('-') && uuid::Uuid::parse_str(&prefix[..36]).is_ok() => {
            &name[37..]
        }
        _ => name,
    };
    (!stripped.is_empty()).then(|| stripped.chars().take(255).collect())
}

/// Rename a deal document
///
/// Sets the display name of a document row (`source` `documents` in
/// `GET /v1/deals/{id}/files`); older documents stored with the photos
/// cannot be renamed.
#[utoipa::path(
    patch,
    path = "/v1/deals/{id}/documents/{document_id}",
    operation_id = "rename_deal_document",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("document_id" = i32, Path, description = "Document id (source `documents`)"),
    ),
    request_body = DocumentRename,
    responses(
        (status = 204, description = "Renamed"),
        (status = 400, description = "Empty or longer than 255 characters", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such document on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn rename_document(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, document_id)): Path<(i64, i32)>,
    JsonBody(body): JsonBody<DocumentRename>,
) -> ApiResult<StatusCode> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::BadRequest("Invalid name".into()));
    }
    require_deal(&state, user.company_id, id).await?;
    let result = sqlx::query("UPDATE deals_documents SET name = ? WHERE id = ? AND deal_id = ?")
        .bind(name)
        .bind(document_id)
        .bind(id)
        .execute(&state.pool)
        .await?;
    if result.rows_affected() == 0 {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT id FROM deals_documents WHERE id = ? AND deal_id = ?")
                .bind(document_id)
                .bind(id)
                .fetch_optional(&state.pool)
                .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound);
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a deal file
///
/// Removes the row (`source` from the file listing) and the stored file.
#[utoipa::path(
    delete,
    path = "/v1/deals/{id}/files/{source}/{file_id}",
    operation_id = "delete_deal_file",
    tag = "deals",
    params(
        ("id" = i64, Path, description = "Deal id"),
        ("source" = FileSource, Path, description = "`images` or `documents`"),
        ("file_id" = i32, Path, description = "Row id within `source`"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "Unknown source", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such file on a live deal of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_file(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path((id, source, file_id)): Path<(i64, String, i32)>,
) -> ApiResult<StatusCode> {
    let table = match source.as_str() {
        "images" => "deals_images",
        "documents" => "deals_documents",
        other => return Err(ApiError::BadRequest(format!("unknown source: {other}"))),
    };
    require_deal(&state, user.company_id, id).await?;
    let url: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT image_url FROM {table} WHERE id = ? AND deal_id = ?"
    )))
    .bind(file_id)
    .bind(id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE id = ?"
    )))
    .bind(file_id)
    .execute(&state.pool)
    .await?;
    remove(&state.storage, &url).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{display_name, is_document};

    #[test]
    fn documents_by_extension() {
        assert!(is_document("Quote.PDF"));
        assert!(is_document("https://x/a.docx?sig=1"));
        assert!(!is_document("photo.jpg"));
        assert!(!is_document("noext"));
    }

    #[test]
    fn display_names_drop_the_upload_prefix() {
        assert_eq!(
            display_name("0f8fad5b-d9cb-469f-a165-70867728950e-Quote.pdf").as_deref(),
            Some("Quote.pdf")
        );
        assert_eq!(display_name(" Quote.pdf ").as_deref(), Some("Quote.pdf"));
        assert_eq!(display_name("  "), None);
    }
}
