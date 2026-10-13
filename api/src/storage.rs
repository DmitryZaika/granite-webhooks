//! File storage for uploaded images (S3 in production).
//!
//! Browsers upload straight to the bucket with a presigned PUT URL
//! (`POST /v1/uploads`): Lambda request bodies are capped at 6 MB, photos
//! are often bigger. The API then attaches the public URL to a record and
//! stores a thumbnail next to the file (`<name>-icon.<ext>`, see
//! [`icon_key`]), like the Remix upload handler did.
//!
//! Configuration (same names as the Remix app): `STORAGE_BUCKET`,
//! `STORAGE_REGION`, `STORAGE_ACCESS_KEY`, `STORAGE_SECRET`. Locally
//! `STORAGE_ENDPOINT` points at an S3-compatible server (path-style
//! requests) and `STORAGE_PUBLIC_URL` is the base of public file URLs
//! (default `https://<bucket>.s3.<region>.amazonaws.com`).

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a presigned download link works.
const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(3600);
/// How long a presigned upload URL stays valid.
pub const UPLOAD_URL_TTL: Duration = Duration::from_mins(15);

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("uploads are not configured")]
    Disabled,
    #[error("storage error: {0}")]
    Backend(String),
}

/// One stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

#[derive(Clone)]
pub enum Storage {
    S3(S3Storage),
    /// In-memory bucket for tests; "uploads" go through [`Storage::put`].
    Memory(MemoryStorage),
    /// No bucket configured: upload routes answer 503.
    Disabled,
}

#[derive(Clone)]
pub struct S3Storage {
    client: aws_sdk_s3::Client,
    bucket: String,
    public_base: String,
}

#[derive(Clone, Default)]
pub struct MemoryStorage {
    objects: Arc<Mutex<HashMap<String, StoredObject>>>,
}

pub const MEMORY_PUBLIC_BASE: &str = "https://files.test/bucket";

impl Storage {
    /// S3 when `STORAGE_BUCKET` is set, otherwise disabled.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let Some(bucket) = var("STORAGE_BUCKET") else {
            return Self::Disabled;
        };
        let region = var("STORAGE_REGION").unwrap_or_else(|| "us-east-2".to_string());
        let mut config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.clone()))
            .credentials_provider(Credentials::new(
                var("STORAGE_ACCESS_KEY").unwrap_or_default(),
                var("STORAGE_SECRET").unwrap_or_default(),
                None,
                None,
                "granite-api-env",
            ));
        if let Some(endpoint) = var("STORAGE_ENDPOINT") {
            config = config.endpoint_url(endpoint).force_path_style(true);
        }
        let public_base = var("STORAGE_PUBLIC_URL")
            .unwrap_or_else(|| format!("https://{bucket}.s3.{region}.amazonaws.com"));
        Self::S3(S3Storage {
            client: aws_sdk_s3::Client::from_conf(config.build()),
            bucket,
            public_base: public_base.trim_end_matches('/').to_string(),
        })
    }

    pub fn memory() -> Self {
        Self::Memory(MemoryStorage::default())
    }

    fn public_base(&self) -> Option<&str> {
        match self {
            Self::S3(s3) => Some(&s3.public_base),
            Self::Memory(_) => Some(MEMORY_PUBLIC_BASE),
            Self::Disabled => None,
        }
    }

    /// Public URL of a key.
    pub fn public_url(&self, key: &str) -> Result<String, StorageError> {
        let base = self.public_base().ok_or(StorageError::Disabled)?;
        Ok(format!("{base}/{key}"))
    }

    /// The key of a URL in this bucket, or `None` for any other URL.
    pub fn key_for_url(&self, url: &str) -> Option<String> {
        let rest = url.strip_prefix(self.public_base()?)?.strip_prefix('/')?;
        (!rest.is_empty() && !rest.contains("..")).then(|| rest.to_string())
    }

    /// A URL the browser can `PUT` the file to, with this exact
    /// `Content-Type` header.
    pub async fn presign_put(&self, key: &str, content_type: &str) -> Result<String, StorageError> {
        match self {
            Self::S3(s3) => {
                let config = PresigningConfig::expires_in(UPLOAD_URL_TTL)
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                let request = s3
                    .client
                    .put_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .content_type(content_type)
                    .presigned(config)
                    .await
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                Ok(request.uri().to_string())
            }
            Self::Memory(_) => Ok(format!("{MEMORY_PUBLIC_BASE}/{key}?upload")),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// A browser-readable URL for a stored file URL. Old records keep private
    /// `s3://bucket/key` URIs: those get a presigned `GET` valid an hour
    /// (shown inline, with `content_type` when given). Any other URL, or a
    /// URI that cannot be signed here, is returned as it is.
    pub async fn readable_url(&self, url: &str, content_type: Option<&str>) -> String {
        let Self::S3(s3) = self else {
            return url.to_string();
        };
        let Some((bucket, key)) = url
            .strip_prefix("s3://")
            .and_then(|rest| rest.split_once('/'))
        else {
            return url.to_string();
        };
        let Ok(config) = PresigningConfig::expires_in(DOWNLOAD_URL_TTL) else {
            return url.to_string();
        };
        let request = s3
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .response_content_disposition("inline")
            .set_response_content_type(content_type.map(str::to_string))
            .presigned(config)
            .await;
        request.map_or_else(|_| url.to_string(), |request| request.uri().to_string())
    }

    pub async fn get(&self, key: &str) -> Result<Option<StoredObject>, StorageError> {
        match self {
            Self::S3(s3) => {
                let result = s3
                    .client
                    .get_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .send()
                    .await;
                match result {
                    Ok(output) => {
                        let content_type = output
                            .content_type()
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        let bytes = output
                            .body
                            .collect()
                            .await
                            .map_err(|err| StorageError::Backend(err.to_string()))?
                            .into_bytes()
                            .to_vec();
                        Ok(Some(StoredObject {
                            bytes,
                            content_type,
                        }))
                    }
                    Err(err) => {
                        if err.as_service_error().is_some_and(
                            aws_sdk_s3::operation::get_object::GetObjectError::is_no_such_key,
                        ) {
                            Ok(None)
                        } else {
                            Err(StorageError::Backend(err.to_string()))
                        }
                    }
                }
            }
            Self::Memory(memory) => Ok(memory.objects.lock().expect("lock").get(key).cloned()),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    pub async fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), StorageError> {
        match self {
            Self::S3(s3) => {
                s3.client
                    .put_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .content_type(content_type)
                    .content_disposition("inline")
                    .body(ByteStream::from(bytes))
                    .send()
                    .await
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                Ok(())
            }
            Self::Memory(memory) => {
                memory.objects.lock().expect("lock").insert(
                    key.to_string(),
                    StoredObject {
                        bytes,
                        content_type: content_type.to_string(),
                    },
                );
                Ok(())
            }
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// A presigned `GET` (an hour) of a key of this bucket, shown inline with
    /// `content_type`; the public URL in the in-memory bucket.
    pub async fn presign_get(&self, key: &str, content_type: &str) -> Result<String, StorageError> {
        self.presign_get_as(key, content_type, None).await
    }

    /// Like [`Storage::presign_get`], with a file name for the browser to
    /// save it under (`inline; filename="..."`).
    pub async fn presign_get_as(
        &self,
        key: &str,
        content_type: &str,
        filename: Option<&str>,
    ) -> Result<String, StorageError> {
        let disposition = filename.map_or_else(
            || "inline".to_string(),
            |name| {
                let ascii: String = name
                    .chars()
                    .map(|c| {
                        if c.is_ascii_graphic() || c == ' ' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .filter(|c| *c != '"' && *c != '\\')
                    .collect();
                let encoded =
                    percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
                format!("inline; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
            },
        );
        match self {
            Self::S3(s3) => {
                let config = PresigningConfig::expires_in(DOWNLOAD_URL_TTL)
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                let request = s3
                    .client
                    .get_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .response_content_disposition(disposition)
                    .response_content_type(content_type)
                    .presigned(config)
                    .await
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                Ok(request.uri().to_string())
            }
            Self::Memory(_) => Ok(format!("{MEMORY_PUBLIC_BASE}/{key}")),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// The content type of a stored object; `None` when it is missing.
    pub async fn head(&self, key: &str) -> Result<Option<String>, StorageError> {
        match self {
            Self::S3(s3) => {
                match s3
                    .client
                    .head_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .send()
                    .await
                {
                    Ok(output) => Ok(Some(
                        output
                            .content_type()
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                    )),
                    Err(err)
                        if err.as_service_error().is_some_and(
                            aws_sdk_s3::operation::head_object::HeadObjectError::is_not_found,
                        ) =>
                    {
                        Ok(None)
                    }
                    Err(err) => Err(StorageError::Backend(err.to_string())),
                }
            }
            Self::Memory(memory) => Ok(memory
                .objects
                .lock()
                .expect("lock")
                .get(key)
                .map(|object| object.content_type.clone())),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// Whether a key of this bucket holds an object.
    pub async fn exists(&self, key: &str) -> Result<bool, StorageError> {
        Ok(self.head(key).await?.is_some())
    }

    /// The bucket and key a stored file URL points at: `s3://bucket/key`, an
    /// S3 HTTPS URL (virtual-host or path style), or a URL of this bucket
    /// (the CRM's `resolveS3Object`). The in-memory bucket ignores bucket names.
    pub fn locate(&self, url: &str) -> Option<(Option<String>, String)> {
        let decode = |key: &str| {
            percent_encoding::percent_decode_str(key)
                .decode_utf8()
                .map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned)
        };
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, key) = rest.split_once('/')?;
            return (!bucket.is_empty() && !key.is_empty())
                .then(|| (Some(bucket.to_string()), decode(key)));
        }
        if let Some(key) = self.key_for_url(url) {
            return Some((None, decode(&key)));
        }
        let rest = url.strip_prefix("https://")?;
        let (host, path) = rest.split_once('/')?;
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let host = host.to_lowercase();
        if !host.ends_with(".amazonaws.com") || path.is_empty() {
            return None;
        }
        if let Some((bucket, _)) = host.split_once(".s3.").or_else(|| host.split_once(".s3-")) {
            return Some((Some(bucket.to_string()), decode(path)));
        }
        if host.starts_with("s3.") || host.starts_with("s3-") {
            let (bucket, key) = path.split_once('/')?;
            return (!key.is_empty()).then(|| (Some(bucket.to_string()), decode(key)));
        }
        None
    }

    /// The file at a stored file URL (see [`Storage::locate`]); `None` when
    /// the URL is not in S3 or the object is missing.
    pub async fn get_url(&self, url: &str) -> Result<Option<StoredObject>, StorageError> {
        let Some((bucket, key)) = self.locate(url) else {
            return Ok(None);
        };
        match (self, bucket) {
            (Self::S3(s3), Some(bucket)) if bucket != s3.bucket => {
                let result = s3.client.get_object().bucket(bucket).key(&key).send().await;
                match result {
                    Ok(output) => {
                        let content_type = output
                            .content_type()
                            .unwrap_or("application/octet-stream")
                            .to_string();
                        let bytes = output
                            .body
                            .collect()
                            .await
                            .map_err(|err| StorageError::Backend(err.to_string()))?
                            .into_bytes()
                            .to_vec();
                        Ok(Some(StoredObject {
                            bytes,
                            content_type,
                        }))
                    }
                    Err(err)
                        if err.as_service_error().is_some_and(
                            aws_sdk_s3::operation::get_object::GetObjectError::is_no_such_key,
                        ) =>
                    {
                        Ok(None)
                    }
                    Err(err) => Err(StorageError::Backend(err.to_string())),
                }
            }
            _ => self.get(&key).await,
        }
    }

    /// Deleting a missing object is not an error.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self {
            Self::S3(s3) => {
                s3.client
                    .delete_object()
                    .bucket(&s3.bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|err| StorageError::Backend(err.to_string()))?;
                Ok(())
            }
            Self::Memory(memory) => {
                memory.objects.lock().expect("lock").remove(key);
                Ok(())
            }
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// Keys stored in the in-memory bucket (tests).
    pub fn memory_keys(&self) -> Vec<String> {
        match self {
            Self::Memory(memory) => {
                let mut keys: Vec<String> = memory
                    .objects
                    .lock()
                    .expect("lock")
                    .keys()
                    .cloned()
                    .collect();
                keys.sort();
                keys
            }
            _ => Vec::new(),
        }
    }
}

/// `stones/abc.jpg` -> `stones/abc-icon.jpg` (Remix `withIconSuffix`).
pub fn icon_key(key: &str) -> String {
    match key.rfind('.') {
        Some(dot) if !key[dot..].contains('/') => format!("{}-icon{}", &key[..dot], &key[dot..]),
        _ => format!("{key}-icon"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_keys_keep_the_extension() {
        assert_eq!(icon_key("stones/a.b/c.jpg"), "stones/a.b/c-icon.jpg");
        assert_eq!(icon_key("stones/abc"), "stones/abc-icon");
        assert_eq!(icon_key("stones.x/abc"), "stones.x/abc-icon");
    }

    #[test]
    fn stored_file_urls_are_located() {
        let storage = Storage::memory();
        assert_eq!(
            storage.locate("s3://inbound/emails/a%20b.heic"),
            Some((Some("inbound".into()), "emails/a b.heic".into()))
        );
        assert_eq!(
            storage.locate(&format!("{MEMORY_PUBLIC_BASE}/emails/x.jpg")),
            Some((None, "emails/x.jpg".into()))
        );
        assert_eq!(
            storage.locate("https://other.s3.us-east-2.amazonaws.com/k/x.jpg?sig=1"),
            Some((Some("other".into()), "k/x.jpg".into()))
        );
        assert_eq!(
            storage.locate("https://s3.us-east-2.amazonaws.com/other/k/x.jpg"),
            Some((Some("other".into()), "k/x.jpg".into()))
        );
        assert_eq!(storage.locate("https://img.example/x.jpg"), None);
        assert_eq!(storage.locate("s3://bucket"), None);
    }

    #[test]
    fn only_urls_of_the_bucket_have_keys() {
        let storage = Storage::memory();
        assert_eq!(
            storage.key_for_url(&format!("{MEMORY_PUBLIC_BASE}/stones/a.png")),
            Some("stones/a.png".into())
        );
        assert_eq!(storage.key_for_url("https://img.example/a.png"), None);
        assert_eq!(storage.key_for_url(&format!("{MEMORY_PUBLIC_BASE}/")), None);
        assert_eq!(
            storage.key_for_url(&format!("{MEMORY_PUBLIC_BASE}/../x")),
            None
        );
        assert_eq!(Storage::Disabled.key_for_url("https://x/y"), None);
    }
}
