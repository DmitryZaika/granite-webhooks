//! Sends an email an employee wrote.
//!
//! The `api` Lambda checks the request,
//! picks the sender and the thread headers and records the message; this
//! route builds the MIME message (bodies, signature, attachments read from
//! S3) and hands it to SES.
//!
//! - `POST /email/send` (Remix key) → `{ "message_id": "…@us-east-2.amazonses.com" }`
//! - `POST /sms/send` (Remix key): see [`sms`]

pub mod body;
pub mod sms;

use crate::amazon::bucket::{CustomClient, S3Bucket};
use crate::axum_helpers::guards::RemixBackend;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use body::outgoing_bodies;
use lambda_http::tracing;
use mail_builder::MessageBuilder;
use serde::{Deserialize, Serialize};

/// Body of `POST /email/send`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutgoingEmail {
    pub from_name: Option<String>,
    pub from_address: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    /// The composer's note: editor HTML or plain text.
    pub body: String,
    /// The sender's signature (HTML or plain text), added below the note.
    pub signature: Option<String>,
    /// `Message-ID` of the message this one answers.
    pub in_reply_to: Option<String>,
    /// `Message-ID`s of the thread, oldest first.
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
    /// A notice the system sends (an appointment confirmation): `body` is
    /// HTML sent as is, without the signature block or the opt-out line.
    #[serde(default)]
    pub transactional: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub content_type: String,
    /// `s3://bucket/key` or the object's `https` URL.
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SentEmail {
    pub message_id: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    Attachment(String),
    InvalidAddress,
    Transport(String),
}

impl IntoResponse for SendError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Attachment(name) => (
                StatusCode::BAD_REQUEST,
                format!("Could not load attachment: {name}"),
            ),
            Self::InvalidAddress => (StatusCode::BAD_REQUEST, "Invalid email address".to_string()),
            Self::Transport(_) => (StatusCode::BAD_GATEWAY, "Email failed to send".to_string()),
        };
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    }
}

pub trait EmailTransport: Send + Sync {
    /// Sends `raw` to `destinations`; returns the message's `Message-ID`.
    fn send_raw(
        &self,
        raw: Vec<u8>,
        destinations: Vec<String>,
    ) -> impl Future<Output = Result<String, String>> + Send;
}

pub struct Ses;

impl EmailTransport for Ses {
    async fn send_raw(&self, raw: Vec<u8>, destinations: Vec<String>) -> Result<String, String> {
        common::amazon::email::send_raw_message(raw, destinations)
            .await
            .map_err(|err| format!("{err:?}"))
    }
}

pub async fn send_handler(
    _: RemixBackend,
    Json(email): Json<OutgoingEmail>,
) -> Result<Json<SentEmail>, SendError> {
    deliver(&CustomClient {}, &Ses, email).await.map(Json)
}

/// Reads the attachments, builds the message and sends it.
pub async fn deliver(
    bucket: &impl S3Bucket,
    transport: &impl EmailTransport,
    email: OutgoingEmail,
) -> Result<SentEmail, SendError> {
    let mut files = Vec::with_capacity(email.attachments.len());
    for attachment in &email.attachments {
        let Some((bucket_name, key)) = s3_location(&attachment.url) else {
            return Err(SendError::Attachment(attachment.filename.clone()));
        };
        let bytes = bucket.read_bytes(&bucket_name, &key).await.map_err(|err| {
            tracing::warn!(error = %err, url = %attachment.url, "email attachment unreadable");
            SendError::Attachment(attachment.filename.clone())
        })?;
        files.push(bytes.to_vec());
    }
    let raw = build_message(&email, files);
    let destinations: Vec<String> = email
        .to
        .iter()
        .chain(&email.cc)
        .chain(&email.bcc)
        .cloned()
        .collect();
    match transport.send_raw(raw, destinations).await {
        Ok(message_id) => Ok(SentEmail { message_id }),
        Err(err) if err.contains("Email address is not verified") => Err(SendError::InvalidAddress),
        Err(err) => {
            tracing::error!(error = %err, "SES send failed");
            Err(SendError::Transport(err))
        }
    }
}

fn bare_message_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// The MIME message: `multipart/alternative` text and HTML, then the
/// attachments (`files[i]` is the content of `email.attachments[i]`). Bcc
/// recipients are only in the envelope.
pub fn build_message(email: &OutgoingEmail, files: Vec<Vec<u8>>) -> Vec<u8> {
    let bodies = if email.transactional {
        body::transactional_bodies(&email.body)
    } else {
        outgoing_bodies(&email.body, email.signature.as_deref())
    };
    let from = match email.from_name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => mail_builder::headers::address::Address::new_address(
            Some(name.to_string()),
            email.from_address.clone(),
        ),
        _ => mail_builder::headers::address::Address::new_address(
            None::<String>,
            email.from_address.clone(),
        ),
    };
    let mut message = MessageBuilder::new()
        .from(from)
        .to(email.to.clone())
        .subject(email.subject.clone())
        .text_body(bodies.text)
        .html_body(bodies.html);
    if !email.cc.is_empty() {
        message = message.cc(email.cc.clone());
    }
    if let Some(in_reply_to) = email
        .in_reply_to
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        message = message.in_reply_to(bare_message_id(in_reply_to));
    }
    let references: Vec<String> = email
        .references
        .iter()
        .map(|id| bare_message_id(id))
        .filter(|id| !id.is_empty())
        .collect();
    if !references.is_empty() {
        message = message.references(references);
    }
    for (attachment, bytes) in email.attachments.iter().zip(files) {
        let content_type = if attachment.content_type.contains('/') {
            attachment.content_type.clone()
        } else {
            "application/octet-stream".to_string()
        };
        message = message.attachment(content_type, attachment.filename.clone(), bytes);
    }
    message
        .write_to_vec()
        .expect("writing to a Vec cannot fail")
}

/// Bucket and key of `s3://bucket/key` or an S3 `https` URL
/// (`bucket.s3[.region].amazonaws.com/key` or `s3[.region].amazonaws.com/bucket/key`).
pub fn s3_location(url: &str) -> Option<(String, String)> {
    let decode = |key: &str| {
        urlencoding::decode(key).map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned)
    };
    if let Some(rest) = url.strip_prefix("s3://") {
        let (bucket, key) = rest.split_once('/')?;
        return (!bucket.is_empty() && !key.is_empty())
            .then(|| (bucket.to_string(), key.to_string()));
    }
    let rest = url.strip_prefix("https://")?;
    let rest = rest.split(['?', '#']).next()?;
    let (host, path) = rest.split_once('/')?;
    if !host.ends_with(".amazonaws.com") || path.is_empty() {
        return None;
    }
    if host.starts_with("s3.") || host.starts_with("s3-") {
        let (bucket, key) = path.split_once('/')?;
        return (!key.is_empty()).then(|| (bucket.to_string(), decode(key)));
    }
    let (bucket, _) = host.split_once(".s3")?;
    Some((bucket.to_string(), decode(path)))
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct Files;

    impl S3Bucket for Files {
        async fn read_bytes(&self, bucket: &str, key: &str) -> Result<Bytes, String> {
            if key.contains("missing") {
                return Err("NoSuchKey".into());
            }
            Ok(Bytes::from(format!("file {bucket}/{key}")))
        }

        async fn send_file(&self, _: &str, _: &str, _: Bytes) -> Result<String, String> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, Vec<String>)>>,
        fail_with: Option<String>,
    }

    impl EmailTransport for Recorder {
        async fn send_raw(
            &self,
            raw: Vec<u8>,
            destinations: Vec<String>,
        ) -> Result<String, String> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            self.sent
                .lock()
                .unwrap()
                .push((String::from_utf8_lossy(&raw).into_owned(), destinations));
            Ok("010f-test@us-east-2.amazonses.com".into())
        }
    }

    fn email() -> OutgoingEmail {
        OutgoingEmail {
            from_name: Some("Dina Dealer".into()),
            from_address: "dina@granite.test".into(),
            to: vec!["nina@example.com".into()],
            cc: vec!["boss@example.com".into()],
            bcc: vec!["hidden@example.com".into()],
            subject: "Your quote".into(),
            body: "<p>Hi Nina</p>".into(),
            signature: Some("Dina".into()),
            in_reply_to: Some("<prev@us-east-2.amazonses.com>".into()),
            references: vec!["first@x".into(), "<prev@us-east-2.amazonses.com>".into()],
            attachments: vec![OutgoingAttachment {
                filename: "quote.pdf".into(),
                content_type: "application/pdf".into(),
                url: "https://bucket.s3.us-east-2.amazonaws.com/emails/a%20b.pdf".into(),
            }],
            transactional: false,
        }
    }

    #[test]
    fn a_transactional_email_has_no_opt_out_line() {
        let notice = OutgoingEmail {
            body: "<p>Your estimate is booked.</p>".into(),
            transactional: true,
            ..email()
        };
        let message = String::from_utf8(build_message(&notice, vec![Vec::new()])).unwrap();
        assert!(message.contains("Your estimate is booked."));
        assert!(!message.contains("rather not receive"));
    }

    #[tokio::test]
    async fn sends_headers_bodies_and_attachments() {
        let transport = Recorder::default();
        let sent = deliver(&Files, &transport, email()).await.unwrap();
        assert_eq!(sent.message_id, "010f-test@us-east-2.amazonses.com");
        let (raw, destinations) = transport.sent.lock().unwrap()[0].clone();
        assert_eq!(
            destinations,
            ["nina@example.com", "boss@example.com", "hidden@example.com"]
        );
        assert!(raw.contains("From: \"Dina Dealer\" <dina@granite.test>"));
        assert!(raw.contains("To: <nina@example.com>"));
        assert!(raw.contains("Cc: <boss@example.com>"));
        assert!(!raw.contains("hidden@example.com"));
        assert!(raw.contains("Subject: Your quote"));
        assert!(raw.contains("In-Reply-To: <prev@us-east-2.amazonses.com>"));
        assert!(raw.contains("References: <first@x> <prev@us-east-2.amazonses.com>"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("quote.pdf"));
        assert!(raw.contains("application/pdf"));
    }

    #[tokio::test]
    async fn unreadable_attachment_is_reported_by_name() {
        let mut email = email();
        email.attachments[0].url = "s3://bucket/emails/missing.pdf".into();
        let err = deliver(&Files, &Recorder::default(), email)
            .await
            .unwrap_err();
        assert_eq!(err, SendError::Attachment("quote.pdf".into()));
        let mut email2 = self::email();
        email2.attachments[0].url = "https://evil.example.com/x.pdf".into();
        let err = deliver(&Files, &Recorder::default(), email2)
            .await
            .unwrap_err();
        assert_eq!(err, SendError::Attachment("quote.pdf".into()));
    }

    #[tokio::test]
    async fn ses_errors_are_mapped() {
        let unverified = Recorder {
            fail_with: Some("MessageRejected: Email address is not verified.".into()),
            ..Recorder::default()
        };
        assert_eq!(
            deliver(&Files, &unverified, email()).await.unwrap_err(),
            SendError::InvalidAddress
        );
        let down = Recorder {
            fail_with: Some("timeout".into()),
            ..Recorder::default()
        };
        assert!(matches!(
            deliver(&Files, &down, email()).await.unwrap_err(),
            SendError::Transport(_)
        ));
    }

    #[test]
    fn s3_locations() {
        assert_eq!(
            s3_location("s3://b/emails/x.pdf"),
            Some(("b".into(), "emails/x.pdf".into()))
        );
        assert_eq!(
            s3_location("https://b.s3.us-east-2.amazonaws.com/emails/a%20b.pdf"),
            Some(("b".into(), "emails/a b.pdf".into()))
        );
        assert_eq!(
            s3_location("https://b.s3.amazonaws.com/k"),
            Some(("b".into(), "k".into()))
        );
        assert_eq!(
            s3_location("https://s3.us-east-2.amazonaws.com/b/k?x=1"),
            Some(("b".into(), "k".into()))
        );
        assert_eq!(s3_location("https://example.com/b/k"), None);
        assert_eq!(s3_location("s3://b"), None);
    }
}
