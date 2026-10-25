//! Email attachments and deals: save an attachment to a deal's files, and
//! which file URLs a deal (or a customer's deals) already has, so the chat
//! can mark attachments that are saved.

use super::schemas::{AddToDeal, DealFileUrls, DealFileUrlsQuery};
use super::sql::can_read_thread;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::routes::deals::queries::{customer_deals, deal_in_company};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sqlx::{AssertSqlSafe, MySqlPool};

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "svg", "heic", "heif", "tiff", "tif",
];

/// `isEmailAttachmentImage`: an image MIME type, HEIC, or an image file name.
pub fn is_image(content_type: &str, content_subtype: &str, filename: &str) -> bool {
    let kind = content_type.trim().to_lowercase();
    let sub = content_subtype.trim().to_lowercase();
    if kind == "image" || kind.starts_with("image/") || sub.contains("heic") || sub.contains("heif")
    {
        return true;
    }
    filename
        .rsplit_once('.')
        .is_some_and(|(_, ext)| IMAGE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// Url, type, subtype, file name, email company and thread of an attachment.
type AttachmentRow = (
    String,
    String,
    Option<String>,
    String,
    Option<i32>,
    Option<String>,
);

/// Url, type, subtype and file name of an attachment the caller may read.
pub(super) async fn readable_attachment(
    pool: &MySqlPool,
    company_id: i32,
    id: i32,
) -> ApiResult<(String, String, String, String)> {
    let row: Option<AttachmentRow> = sqlx::query_as(
        "SELECT ea.url, ea.content_type, ea.content_subtype, ea.filename,
                    e.company_id, e.thread_id
             FROM email_attachments ea JOIN emails e ON e.id = ea.email_id
             WHERE ea.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let Some((url, kind, sub, filename, owner, thread)) = row else {
        return Err(ApiError::NotFound);
    };
    let allowed = match (owner, thread) {
        (Some(owner), _) => owner == company_id,
        (None, Some(thread)) => can_read_thread(&thread, company_id)
            .query()
            .fetch_optional(pool)
            .await?
            .is_some(),
        (None, None) => false,
    };
    if !allowed {
        return Err(ApiError::NotFound);
    }
    Ok((url, kind, sub.unwrap_or_default(), filename))
}

/// Save an attachment to a deal
///
/// Adds the attachment to the deal's photos (images) or documents (other
/// files), once: saving it again changes nothing.
#[crate::openapi::path(
    post,
    path = "/v1/email-attachments/{id}/deal",
    operation_id = "add_email_attachment_to_deal",
    tag = "emails",
    params(("id" = i32, Path, description = "Attachment id")),
    request_body = AddToDeal,
    responses(
        (status = 204, description = "Saved on the deal"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such attachment the caller may read, or no such deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_to_deal(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<AddToDeal>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let (url, kind, sub, filename) = readable_attachment(pool, user.company_id, id).await?;
    if !deal_in_company(pool, user.company_id, body.deal_id).await? {
        return Err(ApiError::NotFound);
    }
    let table = if is_image(&kind, &sub, &filename) {
        "deals_images"
    } else {
        "deals_documents"
    };
    let created_by = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.chars().take(100).collect::<String>());
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (deal_id, image_url, created_by)
         SELECT ?, ?, ? FROM DUAL
         WHERE NOT EXISTS (SELECT 1 FROM {table} WHERE deal_id = ? AND image_url = ?)"
    )))
    .bind(body.deal_id)
    .bind(&url)
    .bind(created_by)
    .bind(body.deal_id)
    .bind(&url)
    .execute(pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List a deal's file URLs
///
/// URLs of the photos and documents of the deal, or of every deal of the
/// customer (by address or name). Attachments whose URL is listed are
/// already saved.
#[crate::openapi::path(
    get,
    path = "/v1/deals/file-urls",
    operation_id = "list_deal_file_urls",
    tag = "deals",
    params(DealFileUrlsQuery),
    responses(
        (status = 200, description = "File URLs", body = DealFileUrls),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 404, description = "No such deal in the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn deal_file_urls(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<DealFileUrlsQuery>,
) -> ApiResult<Json<DealFileUrls>> {
    let pool = &state.pool;
    let deal_ids: Vec<i64> = if let Some(deal_id) = query.deal_id.filter(|id| *id > 0) {
        if !deal_in_company(pool, user.company_id, deal_id).await? {
            return Err(ApiError::NotFound);
        }
        vec![deal_id]
    } else {
        let email = query.customer_email.as_deref().unwrap_or_default().trim();
        let name = query.customer_name.as_deref().unwrap_or_default().trim();
        if email.is_empty() && name.is_empty() {
            Vec::new()
        } else {
            customer_deals(pool, user.company_id, user.id, None, email, name)
                .await?
                .into_iter()
                .map(|deal| deal.id)
                .collect()
        }
    };
    if deal_ids.is_empty() {
        return Ok(Json(DealFileUrls { urls: Vec::new() }));
    }
    let placeholders = vec!["?"; deal_ids.len()].join(",");
    let mut sql = sqlx::query_scalar::<_, String>(AssertSqlSafe(format!(
        "SELECT image_url FROM deals_images WHERE deal_id IN ({placeholders})
         UNION SELECT image_url FROM deals_documents WHERE deal_id IN ({placeholders})"
    )));
    for _ in 0..2 {
        for id in &deal_ids {
            sql = sql.bind(id);
        }
    }
    let urls = sql
        .fetch_all(pool)
        .await?
        .into_iter()
        .filter(|url| !url.trim().is_empty())
        .collect();
    Ok(Json(DealFileUrls { urls }))
}

#[cfg(test)]
mod tests {
    use super::is_image;

    #[test]
    fn images() {
        assert!(is_image("image", "png", "a.png"));
        assert!(is_image("application", "octet-stream", "IMG_1.HEIC"));
        assert!(is_image("image/jpeg", "", "x"));
        assert!(!is_image("application", "pdf", "quote.pdf"));
    }
}

/// A picture sent with an email.
#[derive(Debug, serde::Serialize, crate::openapi::ToSchema)]
pub struct EmailImage {
    /// The attachment.
    pub id: i32,
    /// A URL the browser can show.
    pub url: String,
}

/// List an email's pictures
///
/// Image attachments (HEIC included) of one of the company's emails, for the
/// photo viewer.
#[crate::openapi::path(
    get,
    path = "/v1/emails/{id}/images",
    operation_id = "list_email_images",
    tag = "emails",
    params(("id" = i32, Path, description = "Email id")),
    responses(
        (status = 200, description = "Pictures (none for another company's email)", body = Vec<EmailImage>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn email_images(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<EmailImage>>> {
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT ea.id, ea.url FROM email_attachments ea JOIN emails e ON e.id = ea.email_id
          WHERE ea.email_id = ? AND e.company_id = ?
            AND (LOWER(ea.content_type) = 'image' OR LOWER(ea.filename) LIKE '%.heic'
                 OR LOWER(ea.filename) LIKE '%.heif')",
    )
    .bind(id)
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    let mut images = Vec::with_capacity(rows.len());
    for (id, url) in rows {
        images.push(EmailImage {
            id,
            url: state.storage.readable_url(&url, None).await,
        });
    }
    Ok(Json(images))
}
