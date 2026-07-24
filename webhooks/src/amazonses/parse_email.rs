use bytes::Bytes;
use email_reply_parser::EmailReplyParser;
use mail_parser::{HeaderValue, MessageParser, MessagePart, MimeHeaders, PartType};
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use uuid::Uuid;

use crate::amazon::bucket::S3Bucket;

pub fn filename_to_uuid(original: &str) -> String {
    let path = Path::new(original);

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();

    format!("{}{}", Uuid::new_v4(), ext)
}

pub struct Attachment {
    content_type: String,
    content_subtype: Option<String>,
    filename: String,
    data: Bytes,
}

pub struct UploadedAttachment {
    pub content_type: String,
    pub content_subtype: Option<String>,
    pub filename: String,
    pub url: String,
}

impl Attachment {
    pub async fn to_uploaded_attachment<C: S3Bucket>(self, client: &C) -> UploadedAttachment {
        let filename = filename_to_uuid(&self.filename);
        let url = client
            .send_file("gd-email-attachments", &filename, self.data)
            .await
            .unwrap();
        UploadedAttachment {
            content_type: self.content_type,
            content_subtype: self.content_subtype,
            filename: self.filename,
            url,
        }
    }
}

pub struct ParsedEmail {
    pub subject: Option<String>,
    pub body: String,
    pub sender_email: String,
    pub receiver_email: String,
    pub forward_to_email: Option<String>,
    pub in_reply_to: Option<String>,
    pub message_id: String,
}

impl ParsedEmail {
    pub const fn new(
        subject: Option<String>,
        body: String,
        sender_email: String,
        receiver_email: String,
        forward_to_email: Option<String>,
        in_reply_to: Option<String>,
        message_id: String,
    ) -> Self {
        Self {
            subject,
            body,
            sender_email,
            receiver_email,
            forward_to_email,
            in_reply_to,
            message_id,
        }
    }
}

fn parse_header_value(value: &HeaderValue) -> Option<String> {
    match value {
        HeaderValue::Text(s) => Some(s.to_string()),
        _ => None,
    }
}

pub fn parse_attachment(part: &MessagePart) -> Option<Attachment> {
    // 1. Support Text, HTML, and Binary parts. mail-parser decodes
    // text-based attachments (like .csv or .txt) as Text, not Binary!
    let data = match &part.body {
        PartType::Binary(b) | PartType::InlineBinary(b) => Bytes::copy_from_slice(b),
        PartType::Text(t) | PartType::Html(t) => Bytes::copy_from_slice(t.as_bytes()),
        _ => return None,
    };

    // 2. Fetch Content-Type using the native helper method.
    // Fallback to "application/octet-stream" if the Content-Type is missing.
    let (clean_content_type, content_subtype) = match part.content_type() {
        Some(ct) => (
            ct.c_type.to_string(),
            ct.c_subtype.as_ref().map(std::string::ToString::to_string),
        ),
        None => ("application/octet-stream".to_string(), None),
    };

    // 3. Fetch filename using the native helper method.
    // This internally checks both Content-Disposition and Content-Type attributes for you.
    let filename = part.attachment_name().map_or_else(
        || format!("attachment-{}.bin", Uuid::new_v4()),
        std::string::ToString::to_string,
    );

    Some(Attachment {
        content_type: clean_content_type,
        content_subtype,
        filename,
        data,
    })
}

/// Matches a forward-preamble header line: a non-whitespace word followed
/// by a colon (e.g. `From:`, `От:`, `Date:`, `Subject:`). Used to identify
/// the block of header fields that Gmail inserts after the forward marker,
/// regardless of the sender's UI language.
static FORWARD_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\S+:").unwrap());

/// Strips the forward header block ("---------- Forwarded message ---------"
/// followed by `Label: value` header lines and blank-line separators) from a
/// forwarded email body. Returns the remaining content — the body of the
/// email that was forwarded.
fn extract_forwarded_content(body: &str) -> Option<String> {
    let forward_marker = "---------- Forwarded message ---------";
    let pos = body.find(forward_marker)?;
    let after_marker = &body[pos + forward_marker.len()..];

    let lines: Vec<&str> = after_marker.lines().collect();
    let mut i = 0;

    // Skip any leading blank lines.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }

    // Phase 1: skip the forward-preamble header block — consecutive
    // non-blank lines that look like `Label: value`. Stop at the first
    // blank line or the first line that does not match the pattern.
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || !FORWARD_HEADER_RE.is_match(trimmed) {
            break;
        }
        i += 1;
    }

    // Phase 2: skip blank lines that separate the header block from the
    // forwarded email body.
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }

    if i >= lines.len() {
        return None;
    }

    let content: String = lines[i..].join("\n");
    let content = content.trim().to_string();

    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

pub fn parse_email(email_bytes: &Bytes) -> Result<(ParsedEmail, Vec<Attachment>), String> {
    let message = MessageParser::default()
        .parse(&email_bytes)
        .ok_or("Failed to parse email")?;
    let message_id = message.message_id().ok_or("Failed to parse message ID")?;
    let subject = message.subject();
    let body = message
        .body_text(0)
        .map(std::borrow::Cow::into_owned)
        .unwrap_or_default();
    // Strip angle brackets around URLs to prevent EmailReplyParser from
    // incorrectly treating the closing `>` as an email quote marker.
    static URL_BRACKET_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<(https?://[^\s>]+)>").unwrap());
    let clean_body = URL_BRACKET_RE.replace_all(&body, "$1");
    let reply_body = EmailReplyParser::parse_reply(&clean_body);

    // When a forward contains no new text from the forwarder, EmailReplyParser
    // returns an empty body. Fall back to extracting the last email in the
    // forwarded thread.
    let final_body = if reply_body.trim().is_empty() {
        extract_forwarded_content(&clean_body)
            .map(|content| EmailReplyParser::parse_reply(&content))
            .unwrap_or_default()
    } else {
        reply_body
    };

    let attachments = message.attachments();
    let final_attachments: Vec<Attachment> = attachments.filter_map(parse_attachment).collect();
    let sender_emails = message.from().ok_or("Failed to parse sender email")?;
    let sender_email = sender_emails
        .first()
        .ok_or("Failed to parse sender email")?
        .address
        .as_ref()
        .ok_or("Failed to parse sender email")?
        .to_string();
    let receiver_emails = message.to().ok_or("Failed to parse receiver email")?;
    let receiver_email = receiver_emails
        .first()
        .ok_or("Failed to parse receiver email")?
        .address
        .as_ref()
        .ok_or("Failed to parse receiver email")?
        .to_string();
    let in_reply_to_raw = message.in_reply_to();
    let in_reply_to = parse_header_value(in_reply_to_raw);
    let forward_to_email = if let Some(forwarded_to_email_raw) = message.header("X-Forwarded-To") {
        parse_header_value(forwarded_to_email_raw)
    } else {
        None
    };

    let parsed = ParsedEmail::new(
        subject.map(std::string::ToString::to_string),
        final_body,
        sender_email,
        receiver_email,
        forward_to_email,
        in_reply_to,
        message_id.to_string(),
    );
    Ok((parsed, final_attachments))
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::tests::utils::{read_file_as_bytes, replace_bytes};

    #[test]
    fn test_parse_email() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.subject, Some("Re: COLINS TEST".to_string()));
        const EMAIL_BODY: &str = "Please respond.";
        assert_eq!(parsed_email.body, EMAIL_BODY);
        assert_eq!(parsed_email.sender_email, "colin99delahunty@gmail.com");
        assert_eq!(
            parsed_email.receiver_email,
            "colin.delahunty@granite-manager.com"
        );
        let correct = Some(
            "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000@us-east-2.amazonses.com"
                .to_string(),
        );
        assert_eq!(parsed_email.in_reply_to, correct);
    }

    #[test]
    fn test_parse_email_message_id() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.in_reply_to;
        let correct_message_id = Some(
            "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000@us-east-2.amazonses.com"
                .to_string(),
        );
        assert_eq!(message_id, correct_message_id);
    }

    #[test]
    fn test_parse_email_forward_to() {
        let email_bytes = read_file_as_bytes("src/tests/data/forwarded.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.forward_to_email;
        assert_eq!(message_id.unwrap(), "dema@granitedepotindy.com".to_string());
    }

    #[test]
    fn test_parse_email_forward_to_none() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.forward_to_email;
        assert_eq!(message_id, None);
    }

    #[test]
    fn test_parse_email_message_id_no_amp() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.in_reply_to;
        let correct_message_id = Some(
            "010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000@us-east-2.amazonses.com"
                .to_string(),
        );
        assert_eq!(message_id, correct_message_id);
    }

    #[test]
    fn test_parse_email_message_id_external() {
        let email_bytes = read_file_as_bytes("src/tests/data/external1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.in_reply_to;
        let correct_message_id = None;
        assert_eq!(message_id, correct_message_id);
    }

    #[test]
    fn test_parse_email_attachments() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_attachment_2.eml").unwrap();
        let (_, attachments) = parse_email(&email_bytes).unwrap();
        let attachments = attachments;
        assert_eq!(attachments.len(), 4);
        let expected = [
            ("image", "png", "img_0.png", 134),
            ("image", "jpeg", "img_1.jpg", 376),
            ("image", "png", "img_1.png", 170),
            ("image", "jpeg", "img_0.jpg", 362),
        ];

        for (attachment, (content_type, content_subtype, filename, size)) in
            attachments.iter().zip(expected)
        {
            assert_eq!(attachment.content_type, content_type);
            assert_eq!(
                attachment.content_subtype.as_ref().unwrap(),
                content_subtype
            );
            assert_eq!(attachment.filename, filename);
            assert_eq!(attachment.data.len(), size);
            assert!(!attachment.data.is_empty());
        }
    }
    #[test]
    fn test_parse_email_attachment_no_body() {
        let email_bytes = read_file_as_bytes("src/tests/data/image_only.eml").unwrap();
        let (email, attachments) = parse_email(&email_bytes).unwrap();
        assert_eq!(email.body, "".to_string());
        assert_eq!(attachments.len(), 1);
    }
    #[test]
    fn test_parse_email_with_link() {
        let email_bytes = read_file_as_bytes("src/tests/data/link.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.subject, Some("Link".to_string()));
        assert!(
            parsed_email.body.contains("https://www.reuters.com"),
            "Expected body to contain the link URL, but got: {}",
            parsed_email.body
        );
        assert!(
            parsed_email.body.contains("open this"),
            "Expected body to contain 'open this', but got: {}",
            parsed_email.body
        );
    }
    #[test]
    fn test_parse_email_attachments_filename_only() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_attachment_2.eml").unwrap();
        let clean_bytes = replace_bytes(&email_bytes, " name=", " badkey=").unwrap();
        let (_, attachments) = parse_email(&clean_bytes).unwrap();
        let attachments = attachments;
        assert_eq!(attachments.len(), 4);
        let expected = [
            ("image", "png", "img_0.png", 134),
            ("image", "jpeg", "img_1.jpg", 376),
            ("image", "png", "img_1.png", 170),
            ("image", "jpeg", "img_0.jpg", 362),
        ];

        for (attachment, (content_type, content_subtype, filename, size)) in
            attachments.iter().zip(expected)
        {
            assert_eq!(attachment.content_type, content_type);
            assert_eq!(
                attachment.content_subtype.as_ref().unwrap(),
                content_subtype
            );
            assert_eq!(attachment.filename, filename);
            assert_eq!(attachment.data.len(), size);
            assert!(!attachment.data.is_empty());
        }
    }

    #[test]
    fn test_parse_forward_with_no_new_text() {
        let email_bytes = read_file_as_bytes("src/tests/data/old_forward.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(
            parsed_email.subject,
            Some("Fwd: Granite Depot - Appointment Reminder: Install".to_string())
        );
        assert_eq!(
            parsed_email.body,
            "This was confirmed rescheduled to 7/24 over the phone with Delaney."
        );
    }

    #[test]
    fn test_parse_forward_from_user_no_new_text() {
        let email_bytes = read_file_as_bytes("src/tests/data/forwarded_from_user.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.subject, Some("Fwd:".to_string()));
        assert_eq!(parsed_email.body, "Hello");
    }

    #[test]
    fn test_extract_forwarded_content_basic() {
        let body = concat!(
            "\n",
            "---------- Forwarded message ---------\n",
            "From: Alice <alice@example.com>\n",
            "Date: Thu, Jul 16, 2026 at 12:00 PM\n",
            "Subject: Hello\n",
            "To: Bob <bob@example.com>\n",
            "\n",
            "\n",
            "This is the forwarded content.\n"
        );
        let result = extract_forwarded_content(body);
        assert_eq!(result, Some("This is the forwarded content.".to_string()));
    }

    #[test]
    fn test_extract_forwarded_content_no_marker() {
        let body = "Just a regular email body.";
        let result = extract_forwarded_content(body);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_forwarded_content_empty_after_header() {
        let body = concat!(
            "---------- Forwarded message ---------\n",
            "From: Alice <alice@example.com>\n",
            "Date: Thu, Jul 16, 2026 at 12:00 PM\n",
            "Subject: Test\n",
            "To: Bob <bob@example.com>\n"
        );
        let result = extract_forwarded_content(body);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_forwarded_content_multilingual_headers() {
        // The forward-preamble header block is detected by the `Word:`
        // pattern rather than by a hard-coded list of field names, so it
        // works for any language Gmail localizes into.
        let body = concat!(
            "---------- Forwarded message ---------\n",
            "От: Alice <alice@example.com>\n",
            "Date: Fri, 17 Jul 2026 at 19:06\n",
            "Subject: Hello\n",
            "To: Bob <bob@example.com>\n",
            "\n",
            "\n",
            "This is the forwarded content.\n"
        );
        let result = extract_forwarded_content(body);
        assert_eq!(result, Some("This is the forwarded content.".to_string()));
    }

    #[test]
    fn test_extract_forwarded_content_header_like_body() {
        // A body line like "Re: ..." that appears AFTER blank-line
        // separators must not be treated as a forward-header line.
        let body = concat!(
            "---------- Forwarded message ---------\n",
            "From: Alice <alice@example.com>\n",
            "To: Bob <bob@example.com>\n",
            "\n",
            "\n",
            "Re: This is body content, not a header.\n"
        );
        let result = extract_forwarded_content(body);
        assert_eq!(
            result,
            Some("Re: This is body content, not a header.".to_string())
        );
    }

    #[test]
    fn test_parse_forward_russian_locale() {
        // Forward with Russian-localized header, no new text from forwarder.
        let email_bytes = read_file_as_bytes("src/tests/data/forward_1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.subject, Some("Fwd: First".to_string()));
        assert_eq!(parsed_email.body, "Hello, will Dima see this?");
    }
}
