use super::schemas::UploadFolder;
use super::schemas::{Upload, UploadRequest};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::routes::emails::attachment_token;
use crate::state::AppState;
use crate::thumbnails::IMAGE_CONTENT_TYPES;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use utoipa::IntoParams;

/// Start an upload
///
/// Returns a presigned URL to `PUT` the file to (straight to storage, any
/// size) and the public URL the file will have. Then pass `url` to the
/// record's route, e.g. `PUT /v1/stones/{id}` (`image_url`),
/// `POST /v1/stones/{id}/images` or `POST /v1/deals/{id}/files`; that route
/// also makes the thumbnail. Inventory folders are for admins; `deals` for
/// (and `events`) any employee and also take documents; `quotes` and
/// `template-images` any employee, images only; `emails` for any employee, any
/// file, and the response has the `token` `POST /v1/emails` asks for. Types a
/// browser runs script in (HTML, SVG, XML, JavaScript) are refused everywhere.
#[utoipa::path(
    post,
    path = "/v1/uploads",
    operation_id = "create_upload",
    tag = "uploads",
    request_body = UploadRequest,
    responses(
        (status = 201, description = "Upload URL and public URL", body = Upload),
        (status = 400, description = "Content type not allowed for the folder (HTML, SVG, XML and JavaScript never are), or a malformed body", body = ErrorBody),
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
    let is_deal = matches!(
        request.folder,
        UploadFolder::Deals | UploadFolder::Events
    );
    let is_email = request.folder == UploadFolder::Emails;
    let is_bug_report = request.folder == UploadFolder::BugReports;
    let is_quote = matches!(
        request.folder,
        UploadFolder::Quotes | UploadFolder::TemplateImages
    );
    let is_sms = request.folder == UploadFolder::SmsUploads;
    if !(is_deal
        || is_email
        || is_quote
        || is_bug_report
        || is_sms
        || user.is_admin
        || user.is_superuser)
    {
        return Err(ApiError::Forbidden);
    }
    let content_type = request.content_type.trim().to_lowercase();
    let allowed = IMAGE_CONTENT_TYPES.contains(&content_type.as_str())
        || ((is_deal || is_sms || request.folder == UploadFolder::Documents)
            && DOCUMENT_CONTENT_TYPES.contains(&content_type.as_str()))
        || (is_sms
            && matches!(
                content_type.as_str(),
                "image/avif" | "application/octet-stream"
            ))
        || (request.folder == UploadFolder::SupplierFiles
            && matches!(
                content_type.as_str(),
                "application/pdf" | "text/csv" | "text/tab-separated-values" | "text/plain"
            ))
        || ((is_email || is_bug_report || request.folder == UploadFolder::EmailTemplates)
            && is_mime_type(&content_type)
            && !super::executes_in_browser(&content_type));
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
pub fn extension(file_name: &str, content_type: &str) -> String {
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
            "text/tab-separated-values" => "tsv",
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
    fn script_types() {
        use super::super::executes_in_browser;
        for mime in [
            "image/svg+xml",
            "text/html",
            "application/xhtml+xml",
            "text/javascript",
        ] {
            assert!(executes_in_browser(mime), "{mime}");
        }
        for mime in [
            "image/png",
            "application/pdf",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ] {
            assert!(!executes_in_browser(mime), "{mime}");
        }
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("Photo.JPG", "image/jpeg"), ".jpg");
        assert_eq!(extension("noext", "image/png"), ".png");
        assert_eq!(extension("weird.x/y", "image/webp"), ".webp");
        assert_eq!(extension("a.toolongext", "image/gif"), ".gif");
    }
}

/// Query of `GET /v1/uploads/file`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FileQuery {
    /// Public URL of a file in the bucket.
    pub url: String,
}

/// Fetch a stored file through the API
///
/// Returns the bytes of an image or document in the storage bucket, for the
/// browser to attach it to an email (the bucket sends no CORS headers).
/// Other URLs and other file types (SVG included) are refused. The file is
/// sent as a download that never renders as a page.
#[utoipa::path(
    get,
    path = "/v1/uploads/file",
    operation_id = "fetch_uploaded_file",
    tag = "uploads",
    params(FileQuery),
    responses(
        (status = 200, description = "The file", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 400, description = "Not an image or document in the bucket", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such file", body = ErrorBody),
        (status = 500, description = "Storage error", body = ErrorBody),
        (status = 503, description = "No bucket configured", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn file(
    State(state): State<AppState>,
    _employee: EmployeeUser,
    Query(query): Query<FileQuery>,
) -> ApiResult<impl IntoResponse> {
    let disallowed = || ApiError::BadRequest("Invalid or disallowed URL".into());
    let key = state
        .storage
        .key_for_url(query.url.trim())
        .ok_or_else(disallowed)?;
    let key = percent_decode_str(&key).decode_utf8_lossy().into_owned();
    let object = state.storage.get(&key).await?.ok_or(ApiError::NotFound)?;
    let mime = object
        .content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let image = mime.starts_with("image/") && !super::executes_in_browser(&mime);
    if !(image || DOCUMENT_CONTENT_TYPES[..5].contains(&mime.as_str())) {
        return Err(disallowed());
    }
    Ok((super::stored_file_headers(mime), object.bytes))
}
