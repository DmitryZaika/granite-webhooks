use bytes::Bytes;
use email_reply_parser::EmailReplyParser;
use mail_parser::{Address, HeaderValue, MessageParser, MessagePart, MimeHeaders, PartType};
use regex::Regex;
use std::collections::HashSet;
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

/// One address from a `To:` or `Cc:` header, with its display name kept so the
/// CRM can render "Jane Doe" rather than the bare address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRecipient {
    /// Lowercased, trimmed, bare address — the form stored and compared against.
    pub address: String,
    pub display_name: Option<String>,
}

pub struct ParsedEmail {
    pub subject: Option<String>,
    pub body: String,
    pub html_body: Option<String>,
    pub sender_email: String,
    /// First `To:` address. Retained verbatim so existing callers and the
    /// `emails.receiver_email` column keep their current meaning.
    pub receiver_email: String,
    /// Every `To:` address, in header order.
    pub to_recipients: Vec<ParsedRecipient>,
    /// Every `Cc:` address, in header order.
    pub cc_recipients: Vec<ParsedRecipient>,
    /// Every `Bcc:` address. Normally absent on inbound mail — a BCC'd
    /// recipient's copy does not carry the header — but stored when present.
    pub bcc_recipients: Vec<ParsedRecipient>,
    pub forward_to_email: Option<String>,
    pub in_reply_to: Option<String>,
    /// `References:` chain, oldest first. Used as a threading fallback when
    /// `In-Reply-To` does not match anything we issued.
    pub references: Vec<String>,
    pub message_id: String,
}

/// The single normalization used for every stored or compared address:
/// unwrap `Name <addr>`, drop angle brackets, trim, lowercase.
pub fn normalize_address(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open => &trimmed[open + 1..close],
        _ => trimmed,
    };
    inner.trim().to_lowercase()
}

/// Collect every address in a `To:`/`Cc:` header, skipping entries with no
/// address part and de-duplicating on the normalized form.
fn collect_recipients(address: Option<&Address<'_>>) -> Vec<ParsedRecipient> {
    let Some(address) = address else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for addr in address.iter() {
        let Some(raw) = addr.address.as_ref() else {
            continue;
        };
        let normalized = normalize_address(raw);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(ParsedRecipient {
            address: normalized,
            display_name: addr
                .name
                .as_ref()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty()),
        });
    }
    out
}

/// `References:` arrives as either a single text header or a list, depending on
/// how the sending client folded it.
fn collect_references(value: &HeaderValue<'_>) -> Vec<String> {
    let raw: Vec<String> = match value {
        HeaderValue::Text(text) => vec![text.to_string()],
        HeaderValue::TextList(list) => list.iter().map(std::string::ToString::to_string).collect(),
        _ => Vec::new(),
    };
    let mut seen: HashSet<String> = HashSet::new();
    raw.iter()
        .flat_map(|entry| entry.split_whitespace())
        .map(|entry| {
            entry
                .trim_matches(|c| c == '<' || c == '>')
                .trim()
                .to_string()
        })
        .filter(|entry| !entry.is_empty())
        .filter(|entry| seen.insert(entry.clone()))
        .collect()
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

static OUTLOOK_CID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[cid:[^\]]+\]").unwrap());

fn strip_outlook_cid_markers(body: &str) -> String {
    OUTLOOK_CID_RE
        .replace_all(body, "")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

static ON_WROTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(?:^|\n)On .{10,500}? wrote:\s*").unwrap());

static PARTIAL_ON_WROTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(?:^|\n+)On .+?(?:<\s*)?$").unwrap());

static QUOTED_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^>.*$").unwrap());

static HTML_BLOCKQUOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)(<blockquote[\s\S]*?</blockquote>|<div[^>]*class="[^"]*gmail_quote[^"]*"[^>]*>[\s\S]*)"#,
    )
    .unwrap()
});

static HTML_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

static CID_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<img\b[^>]*\bsrc\s*=\s*["']cid:[^"']*["'][^>]*/?>"#).unwrap()
});

fn strip_html_quoted_content(html: &str) -> String {
    // A gmail_quote wrapper div alone isn't a reliable signal of genuine
    // quoted history — Gmail also wraps multiply-forwarded HTML (e.g.
    // marketing emails re-sent as a reply) in the same div with no
    // `<blockquote>` or "On ... wrote:" attribution. Only strip when a real
    // `<blockquote>` is present, otherwise we'd delete the only content the
    // email has.
    if !html.to_ascii_lowercase().contains("<blockquote") {
        return html.trim().to_string();
    }
    HTML_BLOCKQUOTE_RE.replace_all(html, "").trim().to_string()
}

fn strip_cid_image_tags(html: &str) -> String {
    CID_IMG_RE.replace_all(html, "").into_owned()
}

fn strip_leaked_quote_content(body: &str) -> String {
    let mut text = QUOTED_LINE_RE.replace_all(body, "").to_string();
    if let Some(found) = ON_WROTE_RE.find(&text) {
        text = text[..found.start()].to_string();
    }
    text = PARTIAL_ON_WROTE_RE.replace_all(&text, "").into_owned();
    text.trim().to_string()
}

fn html_body_to_reply_text(html: &str) -> String {
    let without_quotes = HTML_BLOCKQUOTE_RE.replace_all(html, "");
    let text = HTML_TAG_RE.replace_all(&without_quotes, "\n");
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_reply_body(clean_body: &str) -> String {
    let reply_body = EmailReplyParser::parse_reply(clean_body);
    let reply_body = if reply_body.trim().is_empty() {
        extract_forwarded_content(clean_body)
            .map(|content| EmailReplyParser::parse_reply(&content))
            .unwrap_or_default()
    } else {
        reply_body
    };
    strip_leaked_quote_content(&reply_body)
}

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
    let in_reply_to_raw = message.in_reply_to();
    let in_reply_to = parse_header_value(in_reply_to_raw);
    let mut final_body = if in_reply_to.is_some() {
        extract_reply_body(&clean_body)
    } else {
        clean_body.into_owned()
    };
    if final_body.trim().is_empty() && in_reply_to.is_some() {
        if let Some(html) = message.body_html(0) {
            let html_text = html_body_to_reply_text(html.as_ref());
            let clean_html = URL_BRACKET_RE.replace_all(&html_text, "$1");
            final_body = extract_reply_body(&clean_html);
        }
    }
    let final_body = strip_outlook_cid_markers(&final_body);

    let html_body = message
        .body_html(0)
        .map(|html| {
            let cleaned = if in_reply_to.is_some() {
                strip_html_quoted_content(html.as_ref())
            } else {
                html.into_owned()
            };
            strip_cid_image_tags(&cleaned)
        })
        .filter(|html| !HTML_TAG_RE.replace_all(html, "").trim().is_empty());

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
    let to_recipients = collect_recipients(message.to());
    let cc_recipients = collect_recipients(message.cc());
    let bcc_recipients = collect_recipients(message.bcc());
    let references = collect_references(message.references());
    let forward_to_email = if let Some(forwarded_to_email_raw) = message.header("X-Forwarded-To") {
        parse_header_value(forwarded_to_email_raw)
    } else {
        None
    };

    let parsed = ParsedEmail {
        subject: subject.map(std::string::ToString::to_string),
        body: final_body,
        html_body,
        sender_email,
        receiver_email,
        to_recipients,
        cc_recipients,
        bcc_recipients,
        forward_to_email,
        in_reply_to,
        references,
        message_id: message_id.to_string(),
    };
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

    #[test]
    fn test_parse_outlook_bullet_list_first_email() {
        let email_bytes = read_file_as_bytes("src/tests/data/outlook_bullet_list.eml").unwrap();
        let (parsed_email, attachments) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.subject, Some("Templates".to_string()));
        assert!(
            parsed_email.body.contains("3950-2B"),
            "Expected bullet list item in body, got: {}",
            parsed_email.body
        );
        assert!(
            parsed_email
                .body
                .contains("Please let me know your availability"),
            "Expected closing paragraph in body, got: {}",
            parsed_email.body
        );
        assert!(
            !parsed_email.body.contains("[cid:"),
            "Expected cid markers to be stripped, got: {}",
            parsed_email.body
        );
        assert_eq!(parsed_email.in_reply_to, None);
        assert_eq!(attachments.len(), 1);
    }

    #[test]
    fn test_parse_julie_partial_quote_reply() {
        let email_bytes = read_file_as_bytes("src/tests/data/julie_partial_quote.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.body, "Great - thanks for the update!");
    }

    #[test]
    fn test_parse_julie_empty_quote_only_reply() {
        let email_bytes = read_file_as_bytes("src/tests/data/julie_empty_reply.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.body, "");
    }

    #[test]
    fn test_strip_leaked_quote_content_removes_partial_on_wrote_header() {
        let body = "Great - thanks for the update!\n\nOn Fri, Jul 17, 2026 at 12:09 PM Tania Granite Depot <";
        assert_eq!(
            strip_leaked_quote_content(body),
            "Great - thanks for the update!"
        );
    }

    #[test]
    fn test_strip_leaked_quote_content_removes_quote_only_body() {
        let body = "On Fri, Jul 17, 2026 at 5:12 PM Tania Granite Depot <tania@example.com> wrote:\n\n> quoted";
        assert_eq!(strip_leaked_quote_content(body), "");
    }

    #[test]
    fn test_strip_leaked_quote_content_preserves_new_reply_text() {
        let body = "Thanks!\n\nOn Fri, Jul 17, 2026 at 12:09 PM Tania Granite Depot <tania@example.com> wrote:\n\n> quoted";
        assert_eq!(strip_leaked_quote_content(body), "Thanks!");
    }

    #[test]
    fn test_html_body_to_reply_text_extracts_content_before_gmail_quote() {
        let html = concat!(
            "<div>Approved, please proceed.</div>",
            "<div class=\"gmail_quote\"><blockquote>quoted content</blockquote></div>"
        );
        assert_eq!(html_body_to_reply_text(html), "Approved, please proceed.");
    }

    #[test]
    fn test_html_body_to_reply_text_returns_empty_when_only_quote_present() {
        let html =
            "<div><br></div><div class=\"gmail_quote\"><blockquote>quoted only</blockquote></div>";
        assert_eq!(html_body_to_reply_text(html), "");
    }

    #[test]
    fn test_reply_email_still_extracts_new_content() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.body, "Please respond.");
        assert!(parsed_email.in_reply_to.is_some());
    }

    #[test]
    fn test_first_email_does_not_use_reply_parser_on_bullet_list() {
        let email_bytes = read_file_as_bytes("src/tests/data/outlook_bullet_list.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert!(parsed_email.body.contains("3854-2A"));
        assert!(parsed_email.in_reply_to.is_none());
    }

    #[test]
    fn test_html_body_present_for_first_email_with_link() {
        let email_bytes = read_file_as_bytes("src/tests/data/link.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let html_body = parsed_email
            .html_body
            .expect("expected an html body to be captured");
        assert!(
            html_body.contains("href=\"https://www.reuters.com"),
            "Expected html body to keep the real link markup, got: {html_body}"
        );
        assert!(html_body.contains("open this"));
    }

    #[test]
    fn test_html_body_strips_cid_images_on_first_email() {
        let email_bytes = read_file_as_bytes("src/tests/data/outlook_bullet_list.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let html_body = parsed_email
            .html_body
            .expect("expected an html body to be captured");
        assert!(
            html_body.contains("Please let me know your availability"),
            "Expected the real content to survive, got: {html_body}"
        );
        assert!(
            !html_body.contains("cid:"),
            "Expected the broken inline cid image to be stripped, got: {html_body}"
        );
    }

    #[test]
    fn test_html_body_strips_gmail_quote_for_reply() {
        let email_bytes = read_file_as_bytes("src/tests/data/julie_partial_quote.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let html_body = parsed_email
            .html_body
            .expect("expected an html body to be captured");
        assert!(html_body.contains("Great - thanks for the update!"));
        assert!(
            !html_body.contains("gmail_quote"),
            "Expected the quoted thread history to be stripped, got: {html_body}"
        );
        assert!(!html_body.contains("Dasha is handling the slab layout"));
    }

    #[test]
    fn test_html_body_keeps_content_for_forwarded_html_with_no_blockquote() {
        let email_bytes = read_file_as_bytes("src/tests/data/gmail_reply_full_quote.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let html_body = parsed_email
            .html_body
            .expect("expected the forwarded marketing html to be kept");
        assert!(
            html_body.contains("ActionFlow") || html_body.contains("WEBINAR"),
            "Expected the forwarded HTML content to survive since it has no \
             <blockquote>, got: {html_body}"
        );
    }

    #[test]
    fn test_html_body_keeps_content_for_second_forward_with_no_blockquote() {
        let email_bytes = read_file_as_bytes("src/tests/data/gmail_forward_8293.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        let html_body = parsed_email
            .html_body
            .expect("expected the forwarded marketing html to be kept");
        assert!(
            html_body.contains("ActionFlow") || html_body.contains("WEBINAR"),
            "Expected the forwarded HTML content to survive since it has no \
             <blockquote>, got: {html_body}"
        );
    }

    #[test]
    fn test_html_body_none_when_quote_only_reply() {
        let email_bytes = read_file_as_bytes("src/tests/data/julie_empty_reply.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.html_body, None);
    }

    #[test]
    fn test_html_body_none_when_no_html_part() {
        let email_bytes = read_file_as_bytes("src/tests/data/image_only.eml").unwrap();
        let (parsed_email, _) = parse_email(&email_bytes).unwrap();
        assert_eq!(parsed_email.html_body, None);
    }

    /// Several To:, Cc: and Bcc: addresses plus a References: chain — the
    /// shape reply-all and CC visibility depend on.
    const MULTI_RECIPIENT_EML: &[u8] = b"Message-ID: <multi-1@example.com>\r\n\
From: Customer Name <customer@example.com>\r\n\
To: rep@granite-manager.com, Shared Inbox <SHARED@granite-manager.com>\r\n\
Cc: Manager <manager@granite-manager.com>, spouse@example.com\r\n\
Bcc: silent@granite-manager.com, Quiet Partner <QUIET@example.com>\r\n\
References: <root@example.com> <middle@example.com>\r\n\
In-Reply-To: <middle@example.com>\r\n\
Subject: Kitchen quote\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Please confirm the slab.\r\n";

    #[test]
    fn parses_every_to_recipient() {
        let (parsed, _) = parse_email(&Bytes::from_static(MULTI_RECIPIENT_EML)).unwrap();
        let addresses: Vec<&str> = parsed
            .to_recipients
            .iter()
            .map(|r| r.address.as_str())
            .collect();
        assert_eq!(
            addresses,
            vec!["rep@granite-manager.com", "shared@granite-manager.com"]
        );
    }

    #[test]
    fn receiver_email_still_holds_the_first_to_for_backward_compatibility() {
        let (parsed, _) = parse_email(&Bytes::from_static(MULTI_RECIPIENT_EML)).unwrap();
        assert_eq!(parsed.receiver_email, "rep@granite-manager.com");
    }

    #[test]
    fn parses_cc_recipients_with_display_names() {
        let (parsed, _) = parse_email(&Bytes::from_static(MULTI_RECIPIENT_EML)).unwrap();
        assert_eq!(parsed.cc_recipients.len(), 2);
        assert_eq!(
            parsed.cc_recipients[0].address,
            "manager@granite-manager.com"
        );
        assert_eq!(
            parsed.cc_recipients[0].display_name.as_deref(),
            Some("Manager")
        );
        assert_eq!(parsed.cc_recipients[1].address, "spouse@example.com");
        assert_eq!(parsed.cc_recipients[1].display_name, None);
    }

    #[test]
    fn parses_references_chain_without_brackets() {
        let (parsed, _) = parse_email(&Bytes::from_static(MULTI_RECIPIENT_EML)).unwrap();
        assert_eq!(
            parsed.references,
            vec![
                "root@example.com".to_string(),
                "middle@example.com".to_string()
            ]
        );
    }

    #[test]
    fn parses_bcc_recipients_when_present() {
        let (parsed, _) = parse_email(&Bytes::from_static(MULTI_RECIPIENT_EML)).unwrap();
        assert_eq!(parsed.bcc_recipients.len(), 2);
        assert_eq!(
            parsed.bcc_recipients[0].address,
            "silent@granite-manager.com"
        );
        assert_eq!(parsed.bcc_recipients[0].display_name, None);
        assert_eq!(parsed.bcc_recipients[1].address, "quiet@example.com");
        assert_eq!(
            parsed.bcc_recipients[1].display_name.as_deref(),
            Some("Quiet Partner")
        );
    }

    #[test]
    fn bcc_is_empty_when_the_header_is_absent() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed, _) = parse_email(&email_bytes).unwrap();
        assert!(parsed.bcc_recipients.is_empty());
    }

    #[test]
    fn parses_many_cc_and_bcc_addresses() {
        const MANY_EML: &[u8] = b"Message-ID: <many-1@example.com>\r\n\
From: customer@example.com\r\n\
To: rep@granite-manager.com\r\n\
Cc: a@example.com, B@example.com, c@example.com, d@example.com\r\n\
Bcc: e@example.com, F@example.com\r\n\
Subject: Many\r\n\
\r\n\
Body\r\n";
        let (parsed, _) = parse_email(&Bytes::from_static(MANY_EML)).unwrap();
        let cc: Vec<&str> = parsed
            .cc_recipients
            .iter()
            .map(|r| r.address.as_str())
            .collect();
        let bcc: Vec<&str> = parsed
            .bcc_recipients
            .iter()
            .map(|r| r.address.as_str())
            .collect();
        assert_eq!(
            cc,
            vec![
                "a@example.com",
                "b@example.com",
                "c@example.com",
                "d@example.com"
            ]
        );
        assert_eq!(bcc, vec!["e@example.com", "f@example.com"]);
    }

    #[test]
    fn cc_is_empty_when_the_header_is_absent() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let (parsed, _) = parse_email(&email_bytes).unwrap();
        assert!(parsed.cc_recipients.is_empty());
        assert_eq!(parsed.to_recipients.len(), 1);
    }

    #[test]
    fn normalizes_addresses_consistently() {
        assert_eq!(
            normalize_address("Jane Doe <Jane.Doe@Example.COM>"),
            "jane.doe@example.com"
        );
        assert_eq!(normalize_address("  BARE@example.com "), "bare@example.com");
        assert_eq!(
            normalize_address("<wrapped@example.com>"),
            "wrapped@example.com"
        );
    }

    #[test]
    fn deduplicates_an_address_repeated_in_one_header() {
        const DUP_EML: &[u8] = b"Message-ID: <dup-1@example.com>\r\n\
From: customer@example.com\r\n\
To: rep@granite-manager.com, REP@granite-manager.com\r\n\
Subject: Dup\r\n\
\r\n\
Body\r\n";
        let (parsed, _) = parse_email(&Bytes::from_static(DUP_EML)).unwrap();
        assert_eq!(parsed.to_recipients.len(), 1);
    }
}
