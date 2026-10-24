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
    /// Where replies go, when not to the sender.
    #[serde(default)]
    pub reply_to: Vec<String>,
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
    /// The plain-text part of a transactional email; derived from `body`
    /// when missing.
    #[serde(default)]
    pub text: Option<String>,
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
    /// SES is over its sending rate: nothing was sent, try again later.
    Throttled,
    /// SES refused the message: nothing was sent.
    Rejected,
    /// It may or may not have gone out.
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
            Self::Throttled => (
                StatusCode::TOO_MANY_REQUESTS,
                "Email sending is busy, try again in a few minutes".to_string(),
            ),
            Self::Rejected => (StatusCode::BAD_REQUEST, "The email was refused".to_string()),
            Self::Transport(_) => (StatusCode::BAD_GATEWAY, "Email failed to send".to_string()),
        };
        (status, Json(serde_json::json!({ "error": error }))).into_response()
    }
}

/// Why a transport did not send; the text is for logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Refused for its rate or quota: nothing was sent.
    Throttled(String),
    /// Refused: nothing was sent.
    Rejected(String),
    /// It may or may not have gone out.
    Failed(String),
}

pub trait EmailTransport: Send + Sync {
    /// Sends `raw` to `destinations`; returns the message's `Message-ID`.
    fn send_raw(
        &self,
        raw: Vec<u8>,
        destinations: Vec<String>,
    ) -> impl Future<Output = Result<String, TransportError>> + Send;
}

pub struct Ses;

impl EmailTransport for Ses {
    async fn send_raw(
        &self,
        raw: Vec<u8>,
        destinations: Vec<String>,
    ) -> Result<String, TransportError> {
        use common::amazon::email::{SesRefusal, send_raw_message, ses_refusal};
        send_raw_message(raw, destinations).await.map_err(|err| {
            let detail = format!("{err:?}");
            match ses_refusal(&err) {
                Some(SesRefusal::Throttled) => TransportError::Throttled(detail),
                Some(SesRefusal::Rejected) => TransportError::Rejected(detail),
                None => TransportError::Failed(detail),
            }
        })
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
        Err(TransportError::Rejected(err)) if err.contains("Email address is not verified") => {
            Err(SendError::InvalidAddress)
        }
        Err(TransportError::Throttled(err)) => {
            tracing::warn!(error = %err, "SES throttled a send");
            Err(SendError::Throttled)
        }
        Err(TransportError::Rejected(err)) => {
            tracing::warn!(error = %err, "SES refused a send");
            Err(SendError::Rejected)
        }
        Err(TransportError::Failed(err)) => {
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
/// attachments (`files[i]` is the content of `email.attachments[i]`).
///
/// Bcc recipients are only in the envelope; a Bcc-only email (a notice to the
/// whole team) is addressed to `undisclosed-recipients:;`, since SES refuses an
/// empty To header.
pub fn build_message(email: &OutgoingEmail, files: Vec<Vec<u8>>) -> Vec<u8> {
    let bodies = if email.transactional {
        let mut bodies = body::transactional_bodies(&email.body);
        if let Some(text) = email.text.as_deref().filter(|text| !text.trim().is_empty()) {
            bodies.text = text.to_string();
        }
        bodies
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
        .subject(email.subject.clone())
        .text_body(bodies.text)
        .html_body(bodies.html);
    message = if email.to.is_empty() {
        message.header(
            "To",
            mail_builder::headers::raw::Raw::new("undisclosed-recipients:;"),
        )
    } else {
        message.to(email.to.clone())
    };
    if !email.cc.is_empty() {
        message = message.cc(email.cc.clone());
    }
    if !email.reply_to.is_empty() {
        message = message.reply_to(email.reply_to.clone());
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
        fail_with: Option<TransportError>,
    }

    impl EmailTransport for Recorder {
        async fn send_raw(
            &self,
            raw: Vec<u8>,
            destinations: Vec<String>,
        ) -> Result<String, TransportError> {
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
            reply_to: Vec::new(),
            in_reply_to: Some("<prev@us-east-2.amazonses.com>".into()),
            references: vec!["first@x".into(), "<prev@us-east-2.amazonses.com>".into()],
            attachments: vec![OutgoingAttachment {
                filename: "quote.pdf".into(),
                content_type: "application/pdf".into(),
                url: "https://bucket.s3.us-east-2.amazonaws.com/emails/a%20b.pdf".into(),
            }],
            transactional: false,
            text: None,
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

    #[test]
    fn a_transactional_email_keeps_its_own_text_part() {
        let notice = OutgoingEmail {
            body: "<ul><li>Base plan</li><li>Quotes add-on</li></ul>".into(),
            text: Some("- Base plan\n- Quotes add-on".into()),
            transactional: true,
            ..email()
        };
        let message = String::from_utf8(build_message(&notice, vec![Vec::new()])).unwrap();
        assert!(message.contains("- Base plan"));
        assert!(message.contains("<li>Quotes add-on</li>"));
    }

    #[test]
    fn a_bcc_only_email_has_a_non_empty_to_header() {
        // Automation notices go to the reps by Bcc; SES refused the empty To.
        let notice = OutgoingEmail {
            to: Vec::new(),
            cc: Vec::new(),
            attachments: Vec::new(),
            ..email()
        };
        let message = String::from_utf8(build_message(&notice, Vec::new())).unwrap();
        assert!(
            message.contains("To: undisclosed-recipients:;\r\n"),
            "{message}"
        );
        assert!(!message.contains("hidden@example.com"));
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

    // The api's automation notices go to the whole team in Bcc; SES refused
    // every one ("Empty required header 'To'").
    #[tokio::test]
    async fn a_bcc_only_email_is_addressed_to_undisclosed_recipients() {
        let transport = Recorder::default();
        let notice = OutgoingEmail {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: vec!["rep1@example.com".into(), "rep2@example.com".into()],
            ..email()
        };
        deliver(&Files, &transport, notice).await.unwrap();
        let (raw, destinations) = transport.sent.lock().unwrap()[0].clone();
        assert_eq!(destinations, ["rep1@example.com", "rep2@example.com"]);
        assert!(raw.contains("To: undisclosed-recipients:;\r\n"), "{raw}");
        assert!(!raw.contains("rep1@example.com"));
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
            fail_with: Some(TransportError::Rejected(
                "MessageRejected: Email address is not verified.".into(),
            )),
            ..Recorder::default()
        };
        assert_eq!(
            deliver(&Files, &unverified, email()).await.unwrap_err(),
            SendError::InvalidAddress
        );
        for (refusal, expected) in [
            (
                TransportError::Throttled("TooManyRequestsException".into()),
                SendError::Throttled,
            ),
            (
                TransportError::Rejected("MessageRejected".into()),
                SendError::Rejected,
            ),
        ] {
            let refused = Recorder {
                fail_with: Some(refusal),
                ..Recorder::default()
            };
            assert_eq!(
                deliver(&Files, &refused, email()).await.unwrap_err(),
                expected
            );
        }
        assert_eq!(
            SendError::Throttled.into_response().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        let down = Recorder {
            fail_with: Some(TransportError::Failed("timeout".into())),
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
