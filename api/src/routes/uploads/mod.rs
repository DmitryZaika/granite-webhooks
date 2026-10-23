//! Uploads: presigned URLs to put images in the bucket.
//!
//! - `POST /v1/uploads` → a URL to `PUT` one file to, and its public URL
//! - `GET /v1/uploads/file` → a stored image or document's bytes
//!
//! Records take the public URL afterwards (stone image, installed photos,
//! slab photo); [`attach`] checks it and stores the thumbnail.

pub mod handlers;
pub mod schemas;

use crate::error::ApiError;
use crate::state::AppState;
use crate::storage::{Storage, icon_key};
use crate::thumbnails::make_icon;
use axum::http::{HeaderName, header};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::create))
        .routes(routes!(handlers::file))
}

/// A browser runs script in files of this type (`text/html`,
/// `image/svg+xml`, any `+xml`, JavaScript) when it opens them. `mime` is a
/// lowercased `type/subtype` without parameters.
pub fn executes_in_browser(mime: &str) -> bool {
    mime.ends_with("+xml")
        || matches!(
            mime,
            "text/html"
                | "text/xml"
                | "application/xml"
                | "text/xsl"
                | "text/javascript"
                | "text/ecmascript"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-shockwave-flash"
        )
}

/// Headers for the bytes of a stored file the API hands back. They keep the
/// file from ever rendering as a page of the API's origin, where script in
/// an uploaded file would run with the viewer's session: it is only fetched
/// or drawn as an image.
pub fn stored_file_headers(mime: String) -> [(HeaderName, String); 5] {
    [
        (header::CONTENT_TYPE, mime),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox".to_string(),
        ),
        (header::CONTENT_DISPOSITION, "attachment".to_string()),
        (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
    ]
}

/// Accepts `url` for a record when it is a file uploaded to `folder/` of the
/// bucket, and stores its thumbnail (`-icon`) when it is a readable image.
pub async fn attach(
    storage: &Storage,
    url: &str,
    folder: schemas::UploadFolder,
) -> Result<(), ApiError> {
    let key = storage
        .key_for_url(url)
        .filter(|key| key.starts_with(&format!("{}/", folder.as_str())))
        .ok_or_else(|| {
            ApiError::BadRequest(format!("not an uploaded {} file: {url}", folder.as_str()))
        })?;
    let object = storage
        .get(&key)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("file was not uploaded: {url}")))?;
    if let Some(icon) = make_icon(&object.bytes) {
        storage
            .put(&icon_key(&key), icon, &object.content_type)
            .await?;
    }
    Ok(())
}

/// Deletes a file of the bucket and its thumbnail. URLs outside the bucket
/// (old images, seed data) are left alone; failures are logged, not raised,
/// because the record is already gone.
pub async fn remove(storage: &Storage, url: &str) {
    let Some(key) = storage.key_for_url(url) else {
        return;
    };
    for key in [icon_key(&key), key] {
        if let Err(err) = storage.delete(&key).await {
            lambda_http::tracing::warn!(?err, key, "could not delete stored file");
        }
    }
}
