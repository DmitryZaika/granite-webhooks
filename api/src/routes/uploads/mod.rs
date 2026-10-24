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

/// The key of a new upload: `folder/<company id>/<uuid><ext>`.
///
/// The company in the key is what lets [`attach`], [`remove`] and the other
/// routes that take a file URL refuse another company's files: their URLs
/// are public (the showroom shows them).
pub fn new_key(folder: schemas::UploadFolder, company_id: i32, ext: &str) -> String {
    format!(
        "{}/{company_id}/{}{ext}",
        folder.as_str(),
        uuid::Uuid::new_v4()
    )
}

/// The key of `url` when it is a file the company uploaded to `folder/`.
pub fn company_key(
    storage: &Storage,
    url: &str,
    folder: schemas::UploadFolder,
    company_id: i32,
) -> Option<String> {
    storage
        .key_for_url(url)
        .filter(|key| key.starts_with(&format!("{}/{company_id}/", folder.as_str())))
}

/// The key of `url` when it is a file of `folder/` uploaded before keys
/// named the company (`folder/<name>`): it may be any company's.
fn legacy_key(storage: &Storage, url: &str, folder: schemas::UploadFolder) -> Option<String> {
    storage.key_for_url(url).filter(|key| {
        key.strip_prefix(folder.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
    })
}

/// The key of `url` when a record of the company may take it.
///
/// That is a file the company uploaded to `folder/`, or an older upload of
/// the folder whose key names no company (those are never deleted or
/// overwritten, see [`owned_by`]).
pub fn accepted_key(
    storage: &Storage,
    url: &str,
    folder: schemas::UploadFolder,
    company_id: i32,
) -> Option<String> {
    company_key(storage, url, folder, company_id).or_else(|| legacy_key(storage, url, folder))
}

/// Whether `url` is in the bucket under the company (`<folder>/<company
/// id>/...`), so the company may overwrite or delete it.
pub fn owned_by(storage: &Storage, url: &str, company_id: i32) -> bool {
    storage.key_for_url(url).is_some_and(|key| {
        let mut parts = key.splitn(3, '/');
        parts.next().is_some_and(|folder| !folder.is_empty())
            && parts.next() == Some(company_id.to_string().as_str())
            && parts.next().is_some_and(|rest| !rest.is_empty())
    })
}

/// Accepts `url` for a record of the company (see [`accepted_key`]) and
/// stores its thumbnail (`-icon`) when it is a readable image.
pub async fn attach(
    storage: &Storage,
    url: &str,
    folder: schemas::UploadFolder,
    company_id: i32,
) -> Result<(), ApiError> {
    let key = accepted_key(storage, url, folder, company_id).ok_or_else(|| {
        ApiError::BadRequest(format!("not an uploaded {} file: {url}", folder.as_str()))
    })?;
    let object = storage
        .get(&key)
        .await?
        .ok_or_else(|| ApiError::BadRequest(format!("file was not uploaded: {url}")))?;
    let icon = tokio::task::spawn_blocking(move || make_icon(&object.bytes))
        .await
        .ok()
        .flatten();
    if let Some(icon) = icon {
        storage
            .put(&icon_key(&key), icon.bytes, icon.content_type)
            .await?;
    }
    Ok(())
}

/// Deletes a file of the company from the bucket, and its thumbnail.
///
/// Other URLs are left alone: outside the bucket (old images, seed data),
/// older uploads whose key does not name the company (another company's
/// record may hold the same URL), other companies' files. Failures are
/// logged, not raised, because the record is already gone.
pub async fn remove(storage: &Storage, url: &str, company_id: i32) {
    if !owned_by(storage, url, company_id) {
        return;
    }
    let Some(key) = storage.key_for_url(url) else {
        return;
    };
    for key in [icon_key(&key), key] {
        if let Err(err) = storage.delete(&key).await {
            lambda_http::tracing::warn!(?err, key, "could not delete stored file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::schemas::UploadFolder;
    use super::*;
    use crate::storage::MEMORY_PUBLIC_BASE;

    #[test]
    fn keys_name_the_company() {
        let storage = Storage::memory();
        let key = new_key(UploadFolder::Stones, 100, ".jpg");
        assert!(key.starts_with("stones/100/"), "{key}");
        assert_eq!(key.rsplit_once('.').map(|(_, ext)| ext), Some("jpg"));
        let url = format!("{MEMORY_PUBLIC_BASE}/{key}");
        assert_eq!(
            company_key(&storage, &url, UploadFolder::Stones, 100).as_ref(),
            Some(&key)
        );
        assert_eq!(company_key(&storage, &url, UploadFolder::Stones, 101), None);
        assert_eq!(company_key(&storage, &url, UploadFolder::Sinks, 100), None);
        assert!(owned_by(&storage, &url, 100));
        assert!(!owned_by(&storage, &url, 101));
        assert!(!owned_by(&storage, &url, 10));
        let legacy = format!("{MEMORY_PUBLIC_BASE}/stones/abc.jpg");
        assert_eq!(
            legacy_key(&storage, &legacy, UploadFolder::Stones),
            Some("stones/abc.jpg".into())
        );
        assert!(!owned_by(&storage, &legacy, 100));
        assert_eq!(legacy_key(&storage, &url, UploadFolder::Stones), None);
        assert!(!owned_by(
            &storage,
            "https://img.example/stones/100/x.jpg",
            100
        ));
        assert!(!owned_by(
            &storage,
            &format!("{MEMORY_PUBLIC_BASE}/stones/100/"),
            100
        ));
    }
}
