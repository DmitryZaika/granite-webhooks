//! `POST /v1/sms/attachments`: files picked for a text, made ready to send.
//!
//! The browser uploads each file to `sms-uploads/` (`POST /v1/uploads`, a
//! presigned PUT: Lambda bodies stop at 6 MB, phone photos do not). This
//! turns them into the stored attachments `POST /v1/sms/{provider}/messages`
//! sends: pictures upright, at most 1600 px, about 480 KB (HEIC and WebP as
//! JPEG); documents as they are. Replaces the Remix
//! `api.{cloudtalk,ringcentral}.sms.upload-image` routes (`processSmsImage`).

use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::images::{self, ImageError};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Most files in one text.
const MAX_FILES: usize = 10;
/// Biggest file accepted.
const MAX_FILE_BYTES: usize = 15 * 1024 * 1024;

/// Documents a text can carry, by type and by extension.
const DOCUMENT_TYPES: [(&str, &str); 7] = [
    ("application/pdf", "pdf"),
    ("application/msword", "doc"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.ms-excel", "xls"),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("text/plain", "txt"),
    ("text/csv", "csv"),
];

/// A file uploaded to `sms-uploads/`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PickedFile {
    /// Public URL `POST /v1/uploads` gave for it.
    pub url: String,
    /// The name it was picked with (shown to the customer for documents).
    #[serde(default)]
    pub filename: String,
}

/// Body of `POST /v1/sms/attachments`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PrepareAttachments {
    /// One to ten uploaded files.
    pub files: Vec<PickedFile>,
}

/// A file ready to send; pass it in `attachments` of the send call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PreparedAttachment {
    /// Its key, under `sms-attachments/<company id>/`.
    pub s3_key: String,
    /// Its public URL (shown in the composer and the conversation).
    pub url: String,
    /// JPEG, PNG or GIF for pictures; the document's type otherwise.
    pub content_type: String,
    /// The name it was picked with.
    pub filename: String,
    /// Pixel width of a picture; null for documents.
    pub width: Option<u32>,
    /// Pixel height of a picture; null for documents.
    pub height: Option<u32>,
    /// Bytes as stored.
    pub size: usize,
}

/// A file that could not be prepared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct FailedAttachment {
    /// The name it was picked with.
    pub filename: String,
    /// Why: `invalid_upload`, `upload_missing`, `empty_file`,
    /// `file_too_large`, `unsupported_image`, `image_too_large` or `upload_failed`.
    pub error: String,
}

/// Body of the answer: each file is prepared or failed, in order.
#[derive(Debug, Serialize, ToSchema)]
pub struct PreparedAttachments {
    /// Files ready to send.
    pub uploaded: Vec<PreparedAttachment>,
    /// Files that could not be used.
    pub failed: Vec<FailedAttachment>,
}

fn images_enabled() -> bool {
    std::env::var("SMS_IMAGES_ENABLED").map_or(true, |value| {
        !matches!(
            value.trim().to_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        )
    })
}

/// The document type of a file, by its type or else its extension.
fn document_type(content_type: &str, filename: &str) -> Option<(&'static str, &'static str)> {
    let ext = filename.rsplit_once('.').map(|(_, ext)| ext.to_lowercase());
    DOCUMENT_TYPES
        .iter()
        .find(|(kind, _)| *kind == content_type)
        .or_else(|| {
            DOCUMENT_TYPES
                .iter()
                .find(|(_, known)| ext.as_deref() == Some(*known))
        })
        .copied()
}

async fn prepare(
    state: &AppState,
    company_id: i32,
    message: &str,
    file: &PickedFile,
) -> Result<PreparedAttachment, &'static str> {
    let storage = &state.storage;
    // The upload is deleted once copied: only one the company made.
    let key = crate::routes::uploads::company_key(
        storage,
        &file.url,
        crate::routes::uploads::schemas::UploadFolder::SmsUploads,
        company_id,
    )
    .ok_or("invalid_upload")?;
    let object = storage
        .get(&key)
        .await
        .map_err(|_| "upload_failed")?
        .ok_or("upload_missing")?;
    if object.bytes.is_empty() {
        return Err("empty_file");
    }
    if object.bytes.len() > MAX_FILE_BYTES {
        return Err("file_too_large");
    }
    let filename = if file.filename.trim().is_empty() {
        "file".to_string()
    } else {
        file.filename.trim().to_string()
    };
    let (bytes, content_type, ext, width, height) =
        if let Some((kind, ext)) = document_type(&object.content_type, &filename) {
            (object.bytes, kind, ext, None, None)
        } else {
            let source = object.bytes;
            let processed = tokio::task::spawn_blocking(move || images::process_sms_image(&source))
                .await
                .unwrap_or_else(|err| Err(ImageError::Decode(err.to_string())))
                .map_err(|err| match err {
                    ImageError::Unsupported => "unsupported_image",
                    ImageError::TooLarge => "image_too_large",
                    ImageError::Decode(message) => {
                        tracing::warn!(error = %message, "SMS picture could not be read");
                        "unsupported_image"
                    }
                })?;
            (
                processed.bytes,
                processed.content_type,
                processed.ext,
                Some(processed.width),
                Some(processed.height),
            )
        };
    let s3_key = format!(
        "sms-attachments/{company_id}/{message}/{}.{ext}",
        uuid::Uuid::new_v4()
    );
    let size = bytes.len();
    storage
        .put(&s3_key, bytes, content_type)
        .await
        .map_err(|_| "upload_failed")?;
    let url = storage.public_url(&s3_key).map_err(|_| "upload_failed")?;
    if let Err(err) = storage.delete(&key).await {
        tracing::warn!(?err, key, "SMS upload not cleaned up");
    }
    Ok(PreparedAttachment {
        s3_key,
        url,
        content_type: content_type.to_string(),
        filename,
        width,
        height,
        size,
    })
}

/// Prepare files for a text
///
/// Turns files uploaded to `sms-uploads/` into attachments ready to send:
/// pictures upright, at most 1600 px and about 480 KB (HEIC, WebP become
/// JPEG; PNG and GIF stay), documents (PDF, Word, Excel, text, CSV) as they
/// are. The uploads are removed. Each file succeeds or fails on its own.
#[utoipa::path(
    post,
    path = "/v1/sms/attachments",
    operation_id = "prepare_sms_attachments",
    tag = "sms",
    request_body = PrepareAttachments,
    responses(
        (status = 200, description = "Prepared and failed files", body = PreparedAttachments),
        (status = 400, description = "No files, more than ten, or pictures switched off", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 503, description = "No bucket configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn prepare_attachments(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<PrepareAttachments>,
) -> ApiResult<Json<PreparedAttachments>> {
    if !images_enabled() {
        return Err(ApiError::BadRequest("images_disabled".into()));
    }
    if input.files.is_empty() {
        return Err(ApiError::BadRequest("no_images".into()));
    }
    if input.files.len() > MAX_FILES {
        return Err(ApiError::BadRequest("too_many_images".into()));
    }
    let message = uuid::Uuid::new_v4().to_string();
    let mut uploaded = Vec::new();
    let mut failed = Vec::new();
    for file in &input.files {
        match prepare(&state, user.company_id, &message, file).await {
            Ok(prepared) => uploaded.push(prepared),
            Err(error) => failed.push(FailedAttachment {
                filename: file.filename.clone(),
                error: error.to_string(),
            }),
        }
    }
    Ok(Json(PreparedAttachments { uploaded, failed }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_by_type_or_extension() {
        assert_eq!(
            document_type("application/pdf", "x"),
            Some(("application/pdf", "pdf"))
        );
        assert_eq!(
            document_type("application/octet-stream", "Plan.DOCX"),
            Some((
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx"
            ))
        );
        assert_eq!(document_type("image/jpeg", "photo.jpg"), None);
    }
}
