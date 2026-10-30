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

use crate::sigv4::{AWS_PATH, AWS_QUERY, Signer};
use aws_credential_types::Credentials;
use percent_encoding::utf8_percent_encode;
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
    S3(Box<S3Storage>),
    /// In-memory bucket for tests; "uploads" go through [`Storage::put`].
    Memory(MemoryStorage),
    /// No bucket configured: upload routes answer 503.
    Disabled,
}

#[derive(Clone)]
pub struct S3Storage {
    http: reqwest::Client,
    signer: Signer,
    /// `STORAGE_ENDPOINT`: path-style URLs on this server instead of AWS.
    endpoint: Option<String>,
    bucket: String,
    public_base: String,
    /// Buckets that stored URLs never reach (the private signatures bucket):
    /// [`Storage::readable_url`] does not sign them and [`Storage::get_url`]
    /// does not read them, since stored URLs can come from request bodies.
    private_buckets: Vec<String>,
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
        let credentials = Credentials::new(
            var("STORAGE_ACCESS_KEY").unwrap_or_default(),
            var("STORAGE_SECRET").unwrap_or_default(),
            None,
            None,
            "granite-api-env",
        );
        let public_base = var("STORAGE_PUBLIC_URL")
            .unwrap_or_else(|| format!("https://{bucket}.s3.{region}.amazonaws.com"));
        Self::S3(Box::new(S3Storage::new(
            credentials,
            &region,
            var("STORAGE_ENDPOINT"),
            bucket,
            &public_base,
        )))
    }

    /// This storage, with `bucket` out of reach of stored URLs (empty:
    /// unchanged). Read it with [`Storage::get_in`].
    #[must_use]
    pub fn keeping_private(mut self, bucket: &str) -> Self {
        let bucket = bucket.trim();
        if let Self::S3(s3) = &mut self
            && !bucket.is_empty()
            && !s3.private_buckets.iter().any(|b| b == bucket)
        {
            s3.private_buckets.push(bucket.to_string());
        }
        self
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
    // Signing is local, but presigning stays async for its callers.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn presign_put(&self, key: &str, content_type: &str) -> Result<String, StorageError> {
        match self {
            Self::S3(s3) => s3
                .signer
                .presign(
                    "PUT",
                    &s3.object_url(&s3.bucket, key),
                    &[("content-type", content_type)],
                    UPLOAD_URL_TTL,
                )
                .map_err(StorageError::Backend),
            Self::Memory(_) => Ok(format!("{MEMORY_PUBLIC_BASE}/{key}?upload")),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// A browser-readable URL for a stored file URL. Old records keep private
    /// `s3://bucket/key` URIs: those get a presigned `GET` valid an hour
    /// (shown inline, with `content_type` when given). Any other URL, a URI
    /// that cannot be signed here, or one in a private bucket (see
    /// [`Storage::keeping_private`]) is returned as it is.
    // Signing is local, but presigning stays async for its callers.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn readable_url(&self, url: &str, content_type: Option<&str>) -> String {
        let Self::S3(s3) = self else {
            return url.to_string();
        };
        let Some((bucket, key)) = url
            .strip_prefix("s3://")
            .and_then(|rest| rest.split_once('/'))
            .filter(|(bucket, _)| !s3.private_buckets.iter().any(|b| b == bucket))
        else {
            return url.to_string();
        };
        s3.presign_get(bucket, key, "inline", content_type, DOWNLOAD_URL_TTL)
            .unwrap_or_else(|_| url.to_string())
    }

    pub async fn get(&self, key: &str) -> Result<Option<StoredObject>, StorageError> {
        match self {
            Self::S3(s3) => s3.get_object(&s3.bucket, key).await,
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
                let url = s3.object_url(&s3.bucket, key);
                let headers = [
                    ("content-type", content_type),
                    ("content-disposition", "inline"),
                ];
                s3.send("PUT", &url, &headers, bytes.into())
                    .await?
                    .s3_result()
                    .await?;
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

    /// Writes a file once to a private bucket (signatures, call
    /// recordings: `If-None-Match: *`, encrypted, never cached); writing the
    /// same bytes again is fine, other bytes are refused. The bytes are
    /// handed to S3 without a copy (recordings run to 100 MB).
    pub async fn put_private(
        &self,
        bucket: &str,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), StorageError> {
        match self {
            Self::S3(s3) => {
                use base64::Engine as _;
                use sha2::Digest as _;
                let checksum =
                    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&bytes));
                // Shared, not copied: the body and the check below read the same buffer.
                let bytes = axum::body::Bytes::from(bytes);
                let url = s3.object_url(bucket, key);
                let headers = [
                    ("content-type", content_type),
                    ("cache-control", "private, no-store"),
                    ("x-amz-server-side-encryption", "AES256"),
                    ("if-none-match", "*"),
                    ("x-amz-checksum-sha256", checksum.as_str()),
                ];
                let result = s3
                    .send("PUT", &url, &headers, bytes.clone())
                    .await?
                    .s3_result()
                    .await;
                match result {
                    Ok(_) => Ok(()),
                    Err(err) if err.code == "PreconditionFailed" => {
                        let existing = self.get_in(bucket, key).await?;
                        if existing.is_some_and(|object| object.bytes == bytes) {
                            Ok(())
                        } else {
                            Err(StorageError::Backend(
                                "existing signature object integrity check failed".into(),
                            ))
                        }
                    }
                    Err(err) => Err(err.into()),
                }
            }
            Self::Memory(memory) => {
                let mut objects = memory.objects.lock().expect("lock");
                let existing = objects.get(key).map(|existing| existing.bytes == bytes);
                if existing.is_none() {
                    objects.insert(
                        key.to_string(),
                        StoredObject {
                            bytes,
                            content_type: content_type.to_string(),
                        },
                    );
                }
                drop(objects);
                match existing {
                    Some(false) => Err(StorageError::Backend(
                        "existing signature object integrity check failed".into(),
                    )),
                    _ => Ok(()),
                }
            }
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// The bucket blocks all public access (signed documents are private).
    pub async fn verify_private(&self, bucket: &str) -> Result<(), StorageError> {
        let Self::S3(s3) = self else {
            return match self {
                Self::Memory(_) => Ok(()),
                _ => Err(StorageError::Disabled),
            };
        };
        let public = || {
            StorageError::Backend(
                "signature bucket must be private and block all public access".into(),
            )
        };
        let block = s3
            .send(
                "GET",
                &s3.bucket_url(bucket, "publicAccessBlock"),
                &[],
                axum::body::Bytes::new(),
            )
            .await?
            .s3_result()
            .await?
            .text()
            .await?;
        if [
            "BlockPublicAcls",
            "IgnorePublicAcls",
            "BlockPublicPolicy",
            "RestrictPublicBuckets",
        ]
        .iter()
        .any(|setting| xml_text(&block, setting) != Some("true"))
        {
            return Err(public());
        }
        let status = s3
            .send(
                "GET",
                &s3.bucket_url(bucket, "policyStatus"),
                &[],
                axum::body::Bytes::new(),
            )
            .await?
            .s3_result()
            .await;
        match status {
            Ok(status) => {
                if xml_text(&status.text().await?, "IsPublic") != Some("false") {
                    return Err(public());
                }
            }
            Err(err) if err.code == "NoSuchBucketPolicy" => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
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
        self.presign_get_in(None, key, content_type, filename).await
    }

    /// Like [`Storage::presign_get_as`], in another bucket read with the
    /// same credentials (the in-memory bucket ignores bucket names).
    pub async fn presign_get_in(
        &self,
        bucket: Option<&str>,
        key: &str,
        content_type: &str,
        filename: Option<&str>,
    ) -> Result<String, StorageError> {
        self.presign_get_in_for(bucket, key, content_type, filename, DOWNLOAD_URL_TTL)
            .await
    }

    /// [`Storage::presign_get_in`] with a link that works for `ttl` (the
    /// usual one works an hour), e.g. 5 minutes for call recordings.
    // Signing is local, but presigning stays async for its callers.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn presign_get_in_for(
        &self,
        bucket: Option<&str>,
        key: &str,
        content_type: &str,
        filename: Option<&str>,
        ttl: Duration,
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
            Self::S3(s3) => s3
                .presign_get(
                    bucket.unwrap_or(&s3.bucket),
                    key,
                    &disposition,
                    Some(content_type),
                    ttl,
                )
                .map_err(StorageError::Backend),
            Self::Memory(_) => Ok(format!("{MEMORY_PUBLIC_BASE}/{key}")),
            Self::Disabled => Err(StorageError::Disabled),
        }
    }

    /// The content type of a stored object; `None` when it is missing.
    pub async fn head(&self, key: &str) -> Result<Option<String>, StorageError> {
        match self {
            Self::S3(s3) => Ok(s3
                .head_object(&s3.bucket, key)
                .await?
                .map(|response| content_type_of(&response))),
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

    /// The size in bytes of an object in `bucket` (else this bucket); `None`
    /// when it is missing. The in-memory bucket ignores bucket names.
    pub async fn size_in(
        &self,
        bucket: Option<&str>,
        key: &str,
    ) -> Result<Option<u64>, StorageError> {
        match self {
            Self::S3(s3) => Ok(s3
                .head_object(bucket.unwrap_or(&s3.bucket), key)
                .await?
                .map(|response| {
                    response
                        .headers()
                        .get(reqwest::header::CONTENT_LENGTH)
                        .and_then(|length| length.to_str().ok()?.parse().ok())
                        .unwrap_or_default()
                })),
            Self::Memory(memory) => Ok(memory
                .objects
                .lock()
                .expect("lock")
                .get(key)
                .map(|object| u64::try_from(object.bytes.len()).unwrap_or(u64::MAX))),
            Self::Disabled => Err(StorageError::Disabled),
        }
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
    /// the URL is not in S3, is in a private bucket (stored URLs can come
    /// from request bodies; read those with [`Storage::get_in`]), or the
    /// object is missing.
    pub async fn get_url(&self, url: &str) -> Result<Option<StoredObject>, StorageError> {
        let Some((bucket, key)) = self.locate(url) else {
            return Ok(None);
        };
        if let (Self::S3(s3), Some(bucket)) = (self, &bucket)
            && s3.private_buckets.contains(bucket)
        {
            return Ok(None);
        }
        self.get_located(bucket, &key).await
    }

    /// The object at `key` of `bucket`, a private bucket too; `None` when it
    /// is missing. The in-memory bucket ignores bucket names.
    pub async fn get_in(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<StoredObject>, StorageError> {
        self.get_located(Some(bucket.to_string()), key).await
    }

    async fn get_located(
        &self,
        bucket: Option<String>,
        key: &str,
    ) -> Result<Option<StoredObject>, StorageError> {
        match (self, bucket) {
            (Self::S3(s3), Some(bucket)) if bucket != s3.bucket => {
                s3.get_object(&bucket, key).await
            }
            _ => self.get(key).await,
        }
    }

    /// Deleting a missing object is not an error.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self {
            Self::S3(s3) => {
                let url = s3.object_url(&s3.bucket, key);
                s3.send("DELETE", &url, &[], axum::body::Bytes::new())
                    .await?
                    .s3_result()
                    .await?;
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

/// An S3 error response: the HTTP status and S3's error `Code`.
#[derive(Debug)]
struct S3Error {
    status: reqwest::StatusCode,
    code: String,
    message: String,
}

impl From<S3Error> for StorageError {
    fn from(err: S3Error) -> Self {
        Self::Backend(format!("S3 {}: {} {}", err.status, err.code, err.message))
    }
}

impl From<reqwest::Error> for StorageError {
    fn from(err: reqwest::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

trait S3Response: Sized {
    /// The response, or its status and S3 error code when it is not 2xx.
    async fn s3_result(self) -> Result<Self, S3Error>;
}

impl S3Response for reqwest::Response {
    async fn s3_result(self) -> Result<Self, S3Error> {
        let status = self.status();
        if status.is_success() {
            return Ok(self);
        }
        let body = self.text().await.unwrap_or_default();
        Err(S3Error {
            status,
            code: xml_text(&body, "Code").unwrap_or_default().to_string(),
            message: xml_text(&body, "Message").unwrap_or_default().to_string(),
        })
    }
}

/// The text of the first `<tag>` in an S3 XML document.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(xml[start..end].trim())
}

fn content_type_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string()
}

impl S3Storage {
    fn new(
        credentials: Credentials,
        region: &str,
        endpoint: Option<String>,
        bucket: String,
        public_base: &str,
    ) -> Self {
        Self {
            http: common::http::client(),
            signer: Signer::new(credentials, region, "s3"),
            endpoint: endpoint.map(|endpoint| endpoint.trim_end_matches('/').to_string()),
            bucket,
            public_base: public_base.trim_end_matches('/').to_string(),
            private_buckets: Vec::new(),
        }
    }

    /// Virtual-hosted style on AWS, path style on `STORAGE_ENDPOINT`.
    fn bucket_base(&self, bucket: &str) -> String {
        self.endpoint.as_ref().map_or_else(
            || format!("https://{bucket}.s3.{}.amazonaws.com", self.signer.region()),
            |endpoint| format!("{endpoint}/{bucket}"),
        )
    }

    fn object_url(&self, bucket: &str, key: &str) -> String {
        format!(
            "{}/{}",
            self.bucket_base(bucket),
            utf8_percent_encode(key, AWS_PATH)
        )
    }

    /// A bucket subresource such as `?policyStatus`.
    fn bucket_url(&self, bucket: &str, subresource: &str) -> String {
        format!("{}/?{subresource}", self.bucket_base(bucket))
    }

    /// Sends a request signed with `UNSIGNED-PAYLOAD` (HTTPS protects the body).
    async fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: axum::body::Bytes,
    ) -> Result<reqwest::Response, StorageError> {
        let signed = self
            .signer
            .sign_headers(method, url, headers, None)
            .map_err(StorageError::Backend)?;
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method name");
        let mut request = self.http.request(method, url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        for (name, value) in signed {
            request = request.header(name, value);
        }
        Ok(request.body(body).send().await?)
    }

    /// `None` when the key is missing (`NoSuchKey`).
    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<StoredObject>, StorageError> {
        let response = self
            .send(
                "GET",
                &self.object_url(bucket, key),
                &[],
                axum::body::Bytes::new(),
            )
            .await?;
        match response.s3_result().await {
            Ok(response) => Ok(Some(StoredObject {
                content_type: content_type_of(&response),
                bytes: response.bytes().await?.to_vec(),
            })),
            Err(err) if err.code == "NoSuchKey" => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// `None` when the object is missing (HEAD answers a bodiless 404).
    async fn head_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<reqwest::Response>, StorageError> {
        let response = self
            .send(
                "HEAD",
                &self.object_url(bucket, key),
                &[],
                axum::body::Bytes::new(),
            )
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.s3_result().await?))
    }

    /// A presigned `GET` that answers with these `Content-Disposition` and
    /// `Content-Type` headers.
    fn presign_get(
        &self,
        bucket: &str,
        key: &str,
        disposition: &str,
        content_type: Option<&str>,
        ttl: Duration,
    ) -> Result<String, String> {
        let mut url = format!(
            "{}?response-content-disposition={}",
            self.object_url(bucket, key),
            utf8_percent_encode(disposition, AWS_QUERY)
        );
        if let Some(content_type) = content_type {
            url.push_str("&response-content-type=");
            url.extend(utf8_percent_encode(content_type, AWS_QUERY));
        }
        self.signer.presign("GET", &url, &[], ttl)
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

    #[tokio::test]
    async fn stored_urls_never_reach_the_private_bucket() {
        let storage = Storage::S3(Box::new(S3Storage::new(
            Credentials::new("key", "secret", None, None, "test"),
            "us-east-2",
            None,
            "files".into(),
            "https://files.s3.us-east-2.amazonaws.com",
        )))
        .keeping_private(" signatures ");
        let private = "s3://signatures/signatures/9/completed-x.pdf";
        assert_eq!(storage.readable_url(private, None).await, private);
        assert!(storage.get_url(private).await.unwrap().is_none());
        let inbound = storage
            .readable_url("s3://inbound/emails/a.pdf", None)
            .await;
        assert!(
            inbound.starts_with("https://") && inbound.contains("X-Amz-Signature"),
            "{inbound}"
        );
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
