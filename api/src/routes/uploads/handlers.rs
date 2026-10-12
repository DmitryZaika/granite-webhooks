use super::schemas::UploadFolder;
use super::schemas::{Upload, UploadRequest};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::emails::attachment_token;
use crate::state::AppState;
use crate::thumbnails::IMAGE_CONTENT_TYPES;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

/// Start an upload
///
/// Returns a presigned URL to `PUT` the file to (straight to storage, any
/// size) and the public URL the file will have. Then pass `url` to the
/// record's route, e.g. `PUT /v1/stones/{id}` (`image_url`),
/// `POST /v1/stones/{id}/images` or `POST /v1/deals/{id}/files`; that route
/// also makes the thumbnail. Inventory folders are for admins; `deals` for
/// (and `events`) any employee and also take documents; `emails` for any employee, any
/// file, and the response has the `token` `POST /v1/emails` asks for.
#[utoipa::path(
    post,
    path = "/v1/uploads",
    operation_id = "create_upload",
    tag = "uploads",
    request_body = UploadRequest,
    responses(
        (status = 201, description = "Upload URL and public URL", body = Upload),
        (status = 400, description = "Content type not allowed for the folder, or a malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or not an admin for an inventory folder", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured on this server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(request): JsonBody<UploadRequest>,
) -> ApiResult<(StatusCode, Json<Upload>)> {
    let is_deal = matches!(request.folder, UploadFolder::Deals | UploadFolder::Events);
    let is_email = request.folder == UploadFolder::Emails;
    if !(is_deal || is_email || user.is_admin || user.is_superuser) {
        return Err(ApiError::Forbidden);
    }
    let content_type = request.content_type.trim().to_lowercase();
    let allowed = IMAGE_CONTENT_TYPES.contains(&content_type.as_str())
        || (is_deal && DOCUMENT_CONTENT_TYPES.contains(&content_type.as_str()))
        || (is_email && is_mime_type(&content_type));
    if !allowed {
        return Err(ApiError::BadRequest(format!(
            "unsupported content type: {content_type}"
        )));
    }
    let key = format!(
        "{}/{}{}",
        request.folder.as_str(),
        uuid::Uuid::new_v4(),
        extension(&request.file_name, &content_type)
    );
    let upload_url = state.storage.presign_put(&key, &content_type).await?;
    let url = state.storage.public_url(&key)?;
    let token = is_email.then(|| attachment_token::sign(&state.session_secret, &url, user.id));
    Ok((
        StatusCode::CREATED,
        Json(Upload {
            upload_url,
            url,
            token,
        }),
    ))
}

/// `type/subtype` of token characters.
fn is_mime_type(value: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "!#$&-^_.+".contains(c))
    };
    value
        .split_once('/')
        .is_some_and(|(kind, sub)| token(kind) && token(sub))
}

/// Document types a deal may hold besides images.
pub const DOCUMENT_CONTENT_TYPES: &[&str] = &[
    "application/pdf",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/csv",
    "text/plain",
];

/// `.jpg` from `photo.JPG`, else from the content type.
fn extension(file_name: &str, content_type: &str) -> String {
    let from_name = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        });
    let ext = from_name.unwrap_or_else(|| {
        match content_type {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/heic" => "heic",
            "image/heif" => "heif",
            "application/pdf" => "pdf",
            "text/csv" => "csv",
            "text/plain" => "txt",
            "application/zip" => "zip",
            _ => "bin",
        }
        .to_string()
    });
    format!(".{ext}")
}

#[cfg(test)]
mod tests {
    use super::extension;

    #[test]
    fn mime_types() {
        assert!(super::is_mime_type("application/vnd.ms-excel"));
        assert!(super::is_mime_type("video/mp4"));
        assert!(!super::is_mime_type("text"));
        assert!(!super::is_mime_type("text/html; x"));
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("Photo.JPG", "image/jpeg"), ".jpg");
        assert_eq!(extension("noext", "image/png"), ".png");
        assert_eq!(extension("weird.x/y", "image/webp"), ".webp");
        assert_eq!(extension("a.toolongext", "image/gif"), ".gif");
    }
}
