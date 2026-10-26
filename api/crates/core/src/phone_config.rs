//! Granite Phone settings from the environment (`PHONE_*`, `TELNYX_PUBLIC_KEY`,
//! `TELNYX_FALLBACK_TEXML_URL`).

use std::sync::Arc;

/// Where Telnyx calls back when `PHONE_PUBLIC_API_URL` is unset.
pub const DEFAULT_PUBLIC_API_URL: &str = "https://api.granite-manager.com";

/// The TeXML calls go to when our webhook fails and
/// `TELNYX_FALLBACK_TEXML_URL` is unset: "no one can take your call", then
/// a voicemail (in our Telnyx account's media storage).
pub const DEFAULT_FALLBACK_TEXML_URL: &str = "https://api.telnyx.com/v2/media/granite-fallback.xml";

/// Granite Phone settings from the environment.
#[derive(Clone)]
pub struct PhoneConfig {
    /// `PHONE_PUBLIC_API_URL`: this API's public root, the base of the
    /// webhook URLs Telnyx calls (`{base}/v1/webhooks/telnyx/{token}/...`).
    pub public_api_url: Arc<str>,
    /// `PHONE_RECORDINGS_BUCKET`: the private bucket call recordings are
    /// copied to (and the FCM service account may live in).
    pub recordings_bucket: Option<Arc<str>>,
    /// `TELNYX_FALLBACK_TEXML_URL` (default `DEFAULT_FALLBACK_TEXML_URL`):
    /// the TeXML calls go to when our webhook fails, unless the company set
    /// its own.
    pub fallback_texml_url: Option<Arc<str>>,
    /// `TELNYX_PUBLIC_KEY`: Telnyx's Ed25519 public key (base64), to check
    /// webhook signatures.
    pub public_key: Option<Arc<str>>,
    /// `PHONE_ALLOW_UNSIGNED_WEBHOOKS=1`: without `TELNYX_PUBLIC_KEY`, take
    /// Call Control events unsigned (local testing only); otherwise they are
    /// refused.
    pub allow_unsigned_webhooks: bool,
    /// `PHONE_MIN_APP_VERSION`: the oldest app version allowed to call, for
    /// companies that set none.
    pub min_app_version: Option<Arc<str>>,
}

impl Default for PhoneConfig {
    /// The production callback root and nothing else (tests).
    fn default() -> Self {
        Self {
            public_api_url: Arc::from(DEFAULT_PUBLIC_API_URL),
            recordings_bucket: None,
            fallback_texml_url: None,
            public_key: None,
            allow_unsigned_webhooks: false,
            min_app_version: None,
        }
    }
}

impl PhoneConfig {
    /// `PHONE_PUBLIC_API_URL`, `PHONE_RECORDINGS_BUCKET`,
    /// `TELNYX_FALLBACK_TEXML_URL`, `TELNYX_PUBLIC_KEY`,
    /// `PHONE_ALLOW_UNSIGNED_WEBHOOKS` and `PHONE_MIN_APP_VERSION`; blank
    /// values count as unset.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let public_api_url =
            var("PHONE_PUBLIC_API_URL").unwrap_or_else(|| DEFAULT_PUBLIC_API_URL.to_string());
        Self {
            public_api_url: Arc::from(public_api_url.trim_end_matches('/')),
            recordings_bucket: var("PHONE_RECORDINGS_BUCKET").map(Arc::from),
            fallback_texml_url: Some(Arc::from(
                var("TELNYX_FALLBACK_TEXML_URL")
                    .unwrap_or_else(|| DEFAULT_FALLBACK_TEXML_URL.to_string()),
            )),
            public_key: var("TELNYX_PUBLIC_KEY").map(Arc::from),
            allow_unsigned_webhooks: var("PHONE_ALLOW_UNSIGNED_WEBHOOKS").as_deref() == Some("1"),
            min_app_version: var("PHONE_MIN_APP_VERSION").map(Arc::from),
        }
    }
}
