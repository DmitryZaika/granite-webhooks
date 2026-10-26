//! Native e-signatures: configuration and the secrets of the invitation mail.
//!
//! Shared by the invitation and its lifecycle (reminders, notifications).
//! Port of the sealing and token parts of the CRM's
//! `nativeSignatureDelivery.server.ts`.
//!
//! The invitation mail is stored sealed (AES-256-GCM, key derived from
//! `SIGNATURE_INVITATION_SECRET`), and the signing link's token is an HMAC
//! of the request's identity, so neither is ever stored in clear.

use aes_gcm::aead::{Aead, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

/// `NATIVE_SIGNATURES_ENABLED=1`, `SIGNATURE_INVITATION_SECRET` (32 bytes,
/// base64url) and `SIGNATURE_APP_ORIGIN` (the HTTPS origin of `/sign/...`).
#[derive(Clone)]
pub struct NativeSigning {
    secret: String,
    origin: String,
    key: [u8; 32],
}

impl std::fmt::Debug for NativeSigning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSigning")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSigningError {
    #[error("a configured HTTPS signature origin is required")]
    Origin,
    #[error("a 32-byte signature invitation secret is required")]
    Secret,
    #[error("confirmed invitation mail is unavailable")]
    MailUnavailable,
    #[error("invitation has been rotated")]
    Rotated,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

impl NativeSigning {
    /// Checks the secret and origin like the CRM.
    pub fn new(secret: &str, origin: &str) -> Result<Self, NativeSigningError> {
        let parsed = url::Url::parse(origin).map_err(|_| NativeSigningError::Origin)?;
        if parsed.scheme() != "https"
            || parsed.origin().ascii_serialization() != origin
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(NativeSigningError::Origin);
        }
        let valid_secret = secret.len() == 43
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && URL_SAFE_NO_PAD
                .decode(secret)
                .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(&bytes) == secret);
        if !valid_secret {
            return Err(NativeSigningError::Secret);
        }
        Ok(Self {
            secret: secret.to_string(),
            origin: origin.to_string(),
            key: hmac_sha256(secret.as_bytes(), b"native-mail-snapshot-v1"),
        })
    }

    /// From the environment; `None` while native signatures are not enabled.
    pub fn from_env() -> Option<Self> {
        if std::env::var("NATIVE_SIGNATURES_ENABLED").ok().as_deref() != Some("1") {
            return None;
        }
        let secret = std::env::var("SIGNATURE_INVITATION_SECRET").unwrap_or_default();
        let origin = std::env::var("SIGNATURE_APP_ORIGIN").unwrap_or_default();
        match Self::new(&secret, &origin) {
            Ok(signing) => Some(signing),
            Err(err) => {
                lambda_http::tracing::error!(%err, "native signatures are enabled but misconfigured");
                None
            }
        }
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Seals a value: base64url of nonce (12) | ciphertext | tag (16).
    pub fn seal<T: Serialize>(&self, value: &T) -> String {
        let cipher = Aes256Gcm::new(&self.key.into());
        let nonce_bytes: [u8; 12] = rand_bytes();
        let plaintext = serde_json::to_vec(value).expect("serializable");
        let sealed = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
            .expect("AES-GCM encrypts");
        let mut out = nonce_bytes.to_vec();
        out.extend(sealed);
        URL_SAFE_NO_PAD.encode(out)
    }

    /// Opens a sealed value; `None` when it was not sealed with this secret.
    pub fn open<T: for<'de> Deserialize<'de>>(&self, sealed: &str) -> Option<T> {
        let bytes = URL_SAFE_NO_PAD.decode(sealed.trim_end_matches('=')).ok()?;
        if bytes.len() < 12 + 16 {
            return None;
        }
        let cipher = Aes256Gcm::new(&self.key.into());
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
            .ok()?;
        serde_json::from_slice(&plaintext).ok()
    }

    /// The signing link's token for one document of a send.
    pub fn token(
        &self,
        company_id: i64,
        user_id: i64,
        key_hash: &str,
        document_id: &str,
    ) -> String {
        let identity = serde_json::json!([
            "native-invitation-v1",
            company_id,
            user_id,
            key_hash,
            document_id
        ]);
        URL_SAFE_NO_PAD.encode(hmac_sha256(
            self.secret.as_bytes(),
            identity.to_string().as_bytes(),
        ))
    }

    /// The signing page of a document.
    pub fn signing_url(&self, document_id: &str, token: &str) -> String {
        format!("{}/sign/{document_id}?token={token}", self.origin)
    }

    /// The invitation as sent (for its sender and thread) and the signing
    /// link, for reminders and notifications.
    pub async fn lifecycle_mail(
        &self,
        pool: &MySqlPool,
        request_id: i64,
        company_id: i64,
    ) -> Result<LifecycleMail, NativeSigningError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            created_by_user_id: i64,
            logical_send_hash: String,
            encrypted_mail: Option<String>,
            document_id: String,
            token_hash: String,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT CAST(d.created_by_user_id AS SIGNED) AS created_by_user_id, d.logical_send_hash, d.encrypted_mail,
                    nd.document_id, nd.token_hash
               FROM native_signature_delivery_items di
               INNER JOIN native_signature_deliveries d ON d.id = di.delivery_id AND d.company_id = di.company_id
               INNER JOIN native_signature_documents nd ON nd.request_id = di.request_id AND nd.company_id = di.company_id
              WHERE di.request_id = ? AND di.company_id = ? AND d.state IN ('accepted', 'recorded')",
        )
        .bind(request_id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        let row = row.ok_or(NativeSigningError::MailUnavailable)?;
        let sealed = row
            .encrypted_mail
            .ok_or(NativeSigningError::MailUnavailable)?;
        let token = self.token(
            company_id,
            row.created_by_user_id,
            &row.logical_send_hash,
            &row.document_id,
        );
        if sha256_hex(token.as_bytes()) != row.token_hash {
            return Err(NativeSigningError::Rotated);
        }
        let receipt: MailReceipt = self
            .open(&sealed)
            .ok_or(NativeSigningError::MailUnavailable)?;
        Ok(LifecycleMail {
            snapshot: receipt.snapshot,
            signing_url: self.signing_url(&row.document_id, &token),
        })
    }
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    for chunk in bytes.chunks_mut(16) {
        let random = uuid::Uuid::new_v4();
        chunk.copy_from_slice(&random.as_bytes()[..chunk.len()]);
    }
    bytes
}

/// The invitation mail as sent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MailSnapshot {
    pub subject: String,
    pub personalized_body: String,
    pub thread_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub html: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

/// What a delivery stores sealed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailReceipt {
    pub snapshot: MailSnapshot,
    pub message_id: String,
}

#[derive(Debug, Clone)]
pub struct LifecycleMail {
    pub snapshot: MailSnapshot,
    pub signing_url: String,
}

/// A signed request's per-company settings, frozen when it was sent.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenSettings {
    pub reminders: bool,
    pub notifications: Notifications,
    #[serde(default)]
    pub notification_delivery: Option<NotificationDelivery>,
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Notifications {
    pub viewed: bool,
    pub completed: bool,
    pub declined: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationDelivery {
    pub viewed: Option<NotificationPolicy>,
    pub completed: Option<NotificationPolicy>,
    pub declined: Option<NotificationPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPolicy {
    pub recipients: Vec<Recipient>,
    pub subject: String,
    pub text: String,
    pub completed_file: CompletedFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Recipient {
    Sender,
    Signer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletedFile {
    None,
    Link,
    Attachment,
}

/// The lifecycle events a sender can be notified of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
    Viewed,
    Completed,
    Declined,
}

impl LifecycleEvent {
    pub const ALL: [Self; 3] = [Self::Viewed, Self::Completed, Self::Declined];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewed => "viewed",
            Self::Completed => "completed",
            Self::Declined => "declined",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.as_str() == value)
    }
}

impl FrozenSettings {
    /// The settings JSON, checked like the CRM's schema (an enabled
    /// notification needs its policy; only completion may carry the file;
    /// a link policy needs `{{link}}`). Fields the lifecycle does not read
    /// are not checked here.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let decoded = match value {
            serde_json::Value::String(text) => serde_json::from_str(text).ok()?,
            other => other.clone(),
        };
        if decoded.get("parityVerified") != Some(&serde_json::Value::Bool(true)) {
            return None;
        }
        let settings: Self = serde_json::from_value(decoded).ok()?;
        for event in LifecycleEvent::ALL {
            let policy = settings.policy(event);
            if settings.notifies(event) && policy.is_none() {
                return None;
            }
            if let Some(policy) = policy {
                if policy.recipients.is_empty()
                    || policy.subject.is_empty()
                    || policy.text.is_empty()
                {
                    return None;
                }
                if event != LifecycleEvent::Completed
                    && policy.completed_file != CompletedFile::None
                {
                    return None;
                }
                if policy.completed_file == CompletedFile::Link && !policy.text.contains("{{link}}")
                {
                    return None;
                }
            }
        }
        Some(settings)
    }

    pub const fn notifies(&self, event: LifecycleEvent) -> bool {
        match event {
            LifecycleEvent::Viewed => self.notifications.viewed,
            LifecycleEvent::Completed => self.notifications.completed,
            LifecycleEvent::Declined => self.notifications.declined,
        }
    }

    pub fn policy(&self, event: LifecycleEvent) -> Option<&NotificationPolicy> {
        let delivery = self.notification_delivery.as_ref()?;
        match event {
            LifecycleEvent::Viewed => delivery.viewed.as_ref(),
            LifecycleEvent::Completed => delivery.completed.as_ref(),
            LifecycleEvent::Declined => delivery.declined.as_ref(),
        }
    }
}

/// Reminders go out 3, 6 and 10 days after sending, while the request is open.
pub fn reminder_schedule(
    sent_at: chrono::DateTime<chrono::Utc>,
    settings: &FrozenSettings,
    status: &str,
) -> Vec<(i64, chrono::DateTime<chrono::Utc>)> {
    if !settings.reminders || !matches!(status, "sent" | "viewed") {
        return Vec::new();
    }
    [3, 6, 10]
        .into_iter()
        .map(|day| (day, sent_at + chrono::Duration::days(day)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";

    #[test]
    fn seals_and_opens_like_the_crm() {
        let signing = NativeSigning::new(SECRET, "https://sign.example.com").unwrap();
        let sealed = signing.seal(&serde_json::json!({ "a": 1 }));
        assert_eq!(
            signing.open::<serde_json::Value>(&sealed),
            Some(serde_json::json!({ "a": 1 }))
        );
        let other = NativeSigning::new(
            "_wECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
            "https://sign.example.com",
        )
        .unwrap();
        assert_eq!(other.open::<serde_json::Value>(&sealed), None);
    }

    #[test]
    fn rejects_weak_configuration() {
        assert!(NativeSigning::new(SECRET, "http://sign.example.com").is_err());
        assert!(NativeSigning::new(SECRET, "https://sign.example.com/").is_err());
        assert!(NativeSigning::new("short", "https://sign.example.com").is_err());
    }

    #[test]
    fn reminders_on_days_3_6_10_only_while_open() {
        let settings = FrozenSettings::parse(&serde_json::json!({
            "parityVerified": true, "reminders": true, "expiresInDays": null,
            "notifications": { "viewed": false, "completed": false, "declined": false }
        }))
        .unwrap();
        let sent = chrono::DateTime::parse_from_rfc3339("2026-10-03T17:00:00Z")
            .unwrap()
            .to_utc();
        let days: Vec<String> = reminder_schedule(sent, &settings, "sent")
            .iter()
            .map(|(_, at)| at.to_rfc3339())
            .collect();
        assert_eq!(
            days,
            [
                "2026-10-06T17:00:00+00:00",
                "2026-10-09T17:00:00+00:00",
                "2026-10-13T17:00:00+00:00"
            ]
        );
        for status in ["signed", "declined", "expired", "draft"] {
            assert_eq!(reminder_schedule(sent, &settings, status), []);
        }
    }

    #[test]
    fn notification_policies_are_checked() {
        let base = |extra: serde_json::Value| {
            let mut value = serde_json::json!({
                "parityVerified": true, "reminders": true, "expiresInDays": null,
                "notifications": { "viewed": false, "completed": true, "declined": false }
            });
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            value
        };
        assert!(FrozenSettings::parse(&base(serde_json::json!({}))).is_none());
        let viewed_attachment = serde_json::json!({
            "notifications": { "viewed": true, "completed": false, "declined": false },
            "notificationDelivery": { "viewed": { "recipients": ["sender"], "subject": "V", "text": "V {{filename}}", "completedFile": "attachment" } }
        });
        assert!(FrozenSettings::parse(&base(viewed_attachment)).is_none());
    }
}
