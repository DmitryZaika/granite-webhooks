//! `GET /v1/email-attachments/{id}/preview`: a picture attachment the
//! browser can show. iPhone photos (HEIC) become JPEGs, kept in the bucket
//! after the first view; `icon=true` answers a small JPEG for chat tiles.
//! Replaces the Remix `api.email-attachments.$attachmentId.{preview,raw}`.

use super::attachments::{is_image, readable_attachment};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::images::{self, ImageError};
use crate::openapi::IntoParams;
use crate::state::AppState;
use crate::storage::Storage;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// Query of the preview.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PreviewQuery {
    /// `true` for a small JPEG (fits 320 x 320) instead of the full picture.
    #[serde(default)]
    pub icon: bool,
    /// `true` to answer `{"url": ...}` (where the picture is, valid an
    /// hour) instead of redirecting there, for scripts that read the bytes.
    #[serde(default)]
    pub link: bool,
}

/// Where a picture is (`link=true`).
#[derive(Debug, serde::Serialize, crate::openapi::ToSchema)]
pub struct PictureLink {
    /// The picture, valid an hour.
    pub url: String,
}

fn answer(location: &str, link: bool) -> Response {
    if link {
        return (
            [(header::CACHE_CONTROL, "private, no-store")],
            axum::Json(PictureLink {
                url: location.to_string(),
            }),
        )
            .into_response();
    }
    redirect(location, "private, max-age=3600")
}

/// `isHeicEmailAttachment`.
pub fn is_heic(content_type: &str, content_subtype: &str, filename: &str) -> bool {
    let kind = content_type.trim().to_lowercase();
    let sub = content_subtype.trim().to_lowercase();
    let mime = if kind.is_empty() || sub.is_empty() {
        kind
    } else {
        format!("{kind}/{sub}")
    };
    mime.contains("heic")
        || mime.contains("heif")
        || matches!(sub.as_str(), "heic" | "heif")
        || filename
            .rsplit_once('.')
            .is_some_and(|(_, ext)| matches!(ext.to_lowercase().as_str(), "heic" | "heif"))
}

fn mime(content_type: &str, content_subtype: &str) -> Option<String> {
    let kind = content_type.trim().to_lowercase();
    let sub = content_subtype.trim().to_lowercase();
    (!kind.is_empty() && !sub.is_empty()).then(|| format!("{kind}/{sub}"))
}

fn hashed_key(folder: &str, url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    let hex = digest.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    format!("{folder}/{hex}.jpg")
}

fn redirect(location: &str, cache: &'static str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}

async fn source(storage: &Storage, url: &str) -> ApiResult<Vec<u8>> {
    match storage.get_url(url).await? {
        Some(object) => Ok(object.bytes),
        None => Err(ApiError::Upstream("Failed to load attachment".into())),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ImageError> + Send + 'static,
) -> Result<T, ImageError> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|err| Err(ImageError::Decode(err.to_string())))
}

/// Show an image attachment
///
/// For an `<img>`: a JPEG of the attachment's picture. HEIC photos are
/// converted (and kept for later views) and the answer redirects to the
/// JPEG; other pictures redirect to the stored file. With `icon=true` the
/// answer is a small JPEG (fits 320 x 320) itself; with `link=true` it is
/// the picture's URL as JSON instead of a redirect.
#[crate::openapi::path(
    get,
    path = "/v1/email-attachments/{id}/preview",
    operation_id = "preview_email_attachment",
    tag = "emails",
    params(("id" = i32, Path, description = "Attachment id"), PreviewQuery),
    responses(
        (status = 200, description = "The thumbnail (`icon=true`), or where the picture is (`link=true`)",
            content((Vec<u8> = "image/jpeg"), (PictureLink = "application/json"))),
        (status = 302, description = "Redirect to the picture"),
        (status = 400, description = "Not an image", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such attachment the caller may read", body = ErrorBody),
        (status = 500, description = "The picture could not be converted", body = ErrorBody),
        (status = 502, description = "The stored file could not be read", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn preview(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
    Query(query): Query<PreviewQuery>,
) -> ApiResult<Response> {
    let (url, kind, sub, filename) = readable_attachment(&state.pool, user.company_id, id).await?;
    if !is_image(&kind, &sub, &filename) {
        return Err(ApiError::BadRequest("Not an image".into()));
    }
    let storage = &state.storage;
    if query.icon {
        let key = hashed_key("email-attachment-thumbnails", &url);
        let icon = match storage.get(&key).await? {
            Some(stored) if !stored.bytes.is_empty() => stored.bytes,
            _ => {
                let bytes = source(storage, &url).await?;
                let icon = blocking(move || images::thumbnail_jpeg(&bytes))
                    .await
                    .map_err(|err| {
                        tracing::warn!(error = %err, id, "email attachment thumbnail failed");
                        ApiError::Upstream("Failed to create icon".into())
                    })?;
                storage.put(&key, icon.clone(), "image/jpeg").await?;
                icon
            }
        };
        return Ok((
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "private, max-age=604800, immutable"),
            ],
            icon,
        )
            .into_response());
    }
    if !is_heic(&kind, &sub, &filename) {
        let readable = storage
            .readable_url(&url, mime(&kind, &sub).as_deref())
            .await;
        return Ok(answer(&readable, query.link));
    }
    let key = hashed_key("email-attachment-previews", &url);
    if !storage.exists(&key).await? {
        let bytes = source(storage, &url).await?;
        let sniffed = images::sniff(&bytes);
        let converted = blocking(move || images::heic_to_jpeg(&bytes)).await;
        match converted {
            Ok(jpeg) => storage.put(&key, jpeg, "image/jpeg").await?,
            // Named HEIC but really another picture: show it as it is.
            Err(_) if sniffed.is_some_and(|kind| kind != images::Kind::Heic) => {
                let mime = sniffed.map(images::Kind::mime);
                let readable = storage.readable_url(&url, mime).await;
                return Ok(answer(&readable, query.link));
            }
            Err(err) => {
                tracing::warn!(error = %err, id, "HEIC email attachment conversion failed");
                return Err(ApiError::Internal("Failed to convert image".into()));
            }
        }
    }
    let location = storage.presign_get(&key, "image/jpeg").await?;
    Ok(answer(&location, query.link))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heic_by_type_subtype_or_name() {
        assert!(is_heic("image", "heic", "a.jpg"));
        assert!(is_heic("image/heif", "", "a"));
        assert!(is_heic("application", "octet-stream", "IMG_1.HEIC"));
        assert!(!is_heic("image", "jpeg", "a.jpg"));
    }
}
