use bytes::Bytes;
use email_reply_parser::EmailReplyParser;
use mail_parser::{Attribute, HeaderName, HeaderValue, MessageParser, MessagePart, PartType};
use std::borrow::Cow::Borrowed;
use uuid::Uuid;

pub struct Attachment {
    content_type: String,
    content_subtype: Option<String>,
    filename: String,
    data: Bytes,
}

pub struct ParsedEmail {
    pub subject: Option<String>,
    pub body: String,
    pub sender_email: String,
    pub receiver_email: String,
    in_reply_to: Option<String>,
    pub message_id: String,
    pub attachments: Vec<Attachment>,
}

impl ParsedEmail {
    pub const fn new(
        subject: Option<String>,
        body: String,
        sender_email: String,
        receiver_email: String,
        in_reply_to: Option<String>,
        message_id: String,
        attachments: Vec<Attachment>,
    ) -> Self {
        Self {
            subject,
            body,
            sender_email,
            receiver_email,
            in_reply_to,
            message_id,
            attachments,
        }
    }

    pub fn reply_message_id(&self) -> Option<String> {
        let target = self.in_reply_to.clone()?;
        if target.contains("mail.gmail.com") {
            return Some(target);
        }
        let clean = match target.find('@') {
            Some(idx) => &target[..idx],
            None => &target,
        };
        Some(clean.to_string())
    }
}

fn parse_header_value(value: &HeaderValue) -> Option<String> {
    match value {
        HeaderValue::Text(s) => Some(s.to_string()),
        _ => None,
    }
}

fn extract_attribute<'x>(
    attributes: Option<&[Attribute<'x>]>,
    name: std::borrow::Cow<'_, str>,
) -> Option<String> {
    if let Some(attributes) = attributes {
        println!("Attributes: {:?}", attributes);
        for attribute in attributes {
            if attribute.name == name {
                return Some(attribute.value.to_string());
            }
        }
    }
    None
}

fn parse_attachment(part: &MessagePart) -> Option<Attachment> {
    let data = match &part.body {
        PartType::Binary(b) => Bytes::copy_from_slice(b),
        _ => return None,
    };

    let mut content_type: Option<String> = None;
    let mut content_subtype: Option<String> = None;

    let mut filename: Option<String> = None;

    for header in &part.headers {
        match &header.name {
            HeaderName::ContentType => {
                if let HeaderValue::ContentType(ct) = &header.value {
                    println!("Content-Type: {:?}", ct);
                    content_type = Some(ct.c_type.to_string());
                    content_subtype = ct.c_subtype.clone().map(|s| s.into_owned());
                    filename = extract_attribute(ct.attributes(), Borrowed("name"));
                }
            }

            HeaderName::ContentDisposition => {
                if let HeaderValue::ContentType(cd) = &header.value {
                    if filename.is_none() {
                        filename = extract_attribute(cd.attributes(), Borrowed("filename"));
                    }
                }
            }

            _ => {}
        }
    }

    let clean_content_type = match content_type {
        Some(ct) => ct,
        None => return None,
    };

    let default_name = format!("attachment-{}.bin", Uuid::new_v4());

    Some(Attachment {
        content_type: clean_content_type,
        content_subtype: content_subtype,

        filename: filename.unwrap_or_else(|| default_name.to_string()),
        data,
    })
}

pub fn parse_email(email_bytes: &Bytes) -> Result<ParsedEmail, String> {
    let message = MessageParser::default()
        .parse(&email_bytes)
        .ok_or("Failed to parse email")?;
    let message_id = message.message_id().ok_or("Failed to parse message ID")?;
    let subject = message.subject();
    let body = message
        .body_text(0)
        .ok_or("Failed to parse email body")?
        .into_owned();
    let reply_body = EmailReplyParser::parse_reply(&body);
    let attachments = message.attachments();
    let final_attachments = attachments.filter_map(parse_attachment).collect();
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
    Ok(ParsedEmail::new(
        subject.map(std::string::ToString::to_string),
        reply_body,
        sender_email,
        receiver_email,
        in_reply_to,
        message_id.to_string(),
        final_attachments,
    ))
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::tests::utils::read_file_as_bytes;

    #[test]
    fn test_parse_email() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_email1.eml").unwrap();
        let parsed_email = parse_email(&email_bytes).unwrap();
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
        let parsed_email = parse_email(&email_bytes).unwrap();
        let message_id = parsed_email.reply_message_id();
        let correct_message_id =
            Some("010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000".to_string());
        assert_eq!(message_id, correct_message_id);
    }

    #[test]
    fn test_parse_email_attachments() {
        let email_bytes = read_file_as_bytes("src/tests/data/reply_attachment_2.eml").unwrap();
        let parsed_email = parse_email(&email_bytes).unwrap();
        let attachments = parsed_email.attachments;
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
}
