//! `POST /v1/sms/ringcentral/inbound-media`: the pictures and files of an
//! inbound `RingCentral` MMS, kept with the conversation.
//!
//! The `webhooks` Lambda stores the text when `RingCentral` pushes it, then
//! calls this (by its key) with the files' ids: the company's `RingCentral`
//! login and the bucket live here. Each file is downloaded, stored under
//! `sms-attachments/<company id>/` and added to `ringcentral_sms_attachments`.

use crate::auth::job::JobCaller;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::texting::stored_file;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use common::telephony::ringcentral;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::io::Cursor;

/// One file of the message, still on `RingCentral`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InboundMediaFile {
    /// The attachment id under the message's `content/`.
    pub id: String,
    /// MIME type `RingCentral` gives the file (e.g. `image/jpeg`); it picks the
    /// stored file's extension.
    pub content_type: String,
}

/// Body of `POST /v1/sms/ringcentral/inbound-media`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InboundMedia {
    /// The company the message belongs to: its `RingCentral` login is used.
    pub company_id: i32,
    /// The stored `ringcentral_sms` row.
    pub sms_id: i64,
    /// `RingCentral`'s message id.
    pub message_id: i64,
    /// The extension whose message store holds it; the token's own when null.
    pub extension_id: Option<String>,
    /// The message's pictures and files, in the order the customer sent them.
    pub files: Vec<InboundMediaFile>,
}

/// How many files were stored.
#[derive(Debug, Serialize, ToSchema)]
pub struct InboundMediaStored {
    /// Files downloaded and attached; a file that failed is left out.
    pub stored: usize,
}

/// Extension for a stored file of this type.
fn extension(content_type: &str) -> &str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        _ => content_type
            .rsplit('/')
            .next()
            .filter(|ext| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or("bin"),
    }
}

/// Pixel size of a picture the CRM can show.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Store an inbound MMS's files
///
/// Called by the `webhooks` Lambda (job key), not by the CRM: after it saved an
/// inbound `RingCentral` text, it sends the message's file ids. Each file is
/// downloaded with the company's `RingCentral` login, stored under
/// `sms-attachments/<company id>/` and shown with the text in the conversation.
/// Returns how many were stored; 404 when the text is not the company's.
#[crate::openapi::path(
    post,
    path = "/v1/sms/ringcentral/inbound-media",
    operation_id = "store_ringcentral_inbound_media",
    tag = "sms",
    request_body = InboundMedia,
    responses(
        (status = 200, description = "Files stored", body = InboundMediaStored),
        (status = 400, description = "Invalid body", body = ErrorBody),
        (status = 401, description = "Missing or wrong key", body = ErrorBody),
        (status = 404, description = "No such message in the company", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn store_inbound_media(
    State(state): State<AppState>,
    _caller: JobCaller,
    JsonBody(body): JsonBody<InboundMedia>,
) -> ApiResult<Json<InboundMediaStored>> {
    let known: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM ringcentral_sms WHERE id = ? AND company_id = ? AND ringcentral_id = ?",
    )
    .bind(body.sms_id)
    .bind(body.company_id)
    .bind(body.message_id)
    .fetch_optional(&state.pool)
    .await?;
    if known.is_none() {
        return Err(ApiError::NotFound);
    }
    let extension_id = body
        .extension_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("~");
    let mut stored = 0;
    for (position, file) in body.files.iter().enumerate() {
        if file.id.is_empty() || !file.id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let path = format!(
            "account/~/extension/{extension_id}/message-store/{}/content/{}",
            body.message_id, file.id
        );
        let (bytes, _) =
            match ringcentral::get_raw(&state.pool, &state.telephony_http, body.company_id, &path)
                .await
            {
                Ok(file) => file,
                Err(error) => {
                    tracing::error!(
                        company_id = body.company_id,
                        status = error.status,
                        "RingCentral MMS file could not be downloaded"
                    );
                    continue;
                }
            };
        let (bytes, content_type) = stored_file(bytes, file.content_type.clone()).await;
        let is_image = content_type.starts_with("image/");
        let (width, height) = if is_image {
            dimensions(&bytes).map_or((None, None), |(w, h)| (Some(w), Some(h)))
        } else {
            (None, None)
        };
        let ext = extension(&content_type);
        let key = format!(
            "sms-attachments/{}/rc-{}/{}.{ext}",
            body.company_id,
            body.message_id,
            uuid::Uuid::new_v4()
        );
        if let Err(error) = state.storage.put(&key, bytes, &content_type).await {
            tracing::error!(
                company_id = body.company_id,
                ?error,
                "RingCentral MMS file not stored"
            );
            continue;
        }
        let url = state
            .storage
            .public_url(&key)
            .map_err(|err| ApiError::Internal(err.to_string()))?;
        let filename = format!("{}.{ext}", if is_image { "image" } else { "file" });
        sqlx::query(
            "INSERT INTO ringcentral_sms_attachments
                    (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height, position)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(body.sms_id)
        .bind(&content_type)
        .bind(filename)
        .bind(&key)
        .bind(url)
        .bind(width)
        .bind(height)
        .bind(u32::try_from(position).unwrap_or(u32::MAX))
        .execute(&state.pool)
        .await?;
        stored += 1;
    }
    Ok(Json(InboundMediaStored { stored }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_follows_the_type() {
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("video/mp4"), "mp4");
        assert_eq!(extension("application/vnd.ms-excel"), "bin");
        assert_eq!(extension(""), "bin");
    }

    #[test]
    fn dimensions_of_a_png() {
        let mut png = Vec::new();
        image::RgbImage::new(3, 2)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(dimensions(&png), Some((3, 2)));
        assert_eq!(dimensions(b"not a picture"), None);
    }
}
