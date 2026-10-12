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
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::create))
        .routes(routes!(handlers::file))
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
