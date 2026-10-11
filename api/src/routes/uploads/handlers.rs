use super::schemas::{Upload, UploadRequest};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::thumbnails::IMAGE_CONTENT_TYPES;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

/// Start an image upload
///
/// Returns a presigned URL to `PUT` the file to (straight to storage, any
/// size) and the public URL the file will have. Then pass `url` to the
/// record's route, e.g. `PUT /v1/stones/{id}` (`image_url`) or
/// `POST /v1/stones/{id}/images`; that route also makes the thumbnail.
#[utoipa::path(
    post,
    path = "/v1/uploads",
    operation_id = "create_upload",
    tag = "uploads",
    request_body = UploadRequest,
    responses(
        (status = 201, description = "Upload URL and public URL", body = Upload),
        (status = 400, description = "Not an image content type, or a malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "Uploads are not configured on this server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    JsonBody(request): JsonBody<UploadRequest>,
) -> ApiResult<(StatusCode, Json<Upload>)> {
    let content_type = request.content_type.trim().to_lowercase();
    if !IMAGE_CONTENT_TYPES.contains(&content_type.as_str()) {
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
    Ok((StatusCode::CREATED, Json(Upload { upload_url, url })))
}

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
            _ => "heif",
        }
        .to_string()
    });
    format!(".{ext}")
}

#[cfg(test)]
mod tests {
    use super::extension;

    #[test]
    fn extensions() {
        assert_eq!(extension("Photo.JPG", "image/jpeg"), ".jpg");
        assert_eq!(extension("noext", "image/png"), ".png");
        assert_eq!(extension("weird.x/y", "image/webp"), ".webp");
        assert_eq!(extension("a.toolongext", "image/gif"), ".gif");
    }
}
