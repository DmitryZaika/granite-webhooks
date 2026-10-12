use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Where an upload goes; each record type takes files of its own folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum UploadFolder {
    /// Stone images, installed stone photos and slab photos.
    Stones,
    /// Sink images and installed sink photos.
    Sinks,
    /// Faucet images and installed faucet photos.
    Faucets,
    /// Photos and documents attached to a deal (any employee may upload).
    Deals,
    /// Attachments of an email being sent (any employee, any file type).
    Emails,
}

impl UploadFolder {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stones => "stones",
            Self::Sinks => "sinks",
            Self::Faucets => "faucets",
            Self::Deals => "deals",
            Self::Emails => "emails",
        }
    }
}

/// Body of `POST /v1/uploads`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadRequest {
    /// Folder of the record the file is for.
    pub folder: UploadFolder,
    /// Original file name; only its extension is kept.
    pub file_name: String,
    /// MIME type of the file: `image/jpeg`, `image/png`, `image/webp`,
    /// `image/gif`, `image/heic` or `image/heif`; for `deals` also PDF, Word,
    /// Excel, CSV and plain text; for `emails` any type. Send the same value
    /// as the `Content-Type` header of the `PUT`.
    pub content_type: String,
}

/// Where to upload, and the URL to give the record afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Upload {
    /// `PUT` the file body here (valid 15 minutes) with the `Content-Type`
    /// given in the request.
    pub upload_url: String,
    /// Public URL of the file once uploaded; pass it to the record's route.
    pub url: String,
    /// `emails` only: proof the caller uploaded `url`; send it with the
    /// attachment to `POST /v1/emails`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}
