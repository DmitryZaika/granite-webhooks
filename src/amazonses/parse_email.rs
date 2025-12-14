use bytes::Bytes;
use email_reply_parser::EmailReplyParser;
use mail_parser::{HeaderValue, MessageParser};

pub struct ParsedEmail {
    pub subject: Option<String>,
    pub body: String,
    pub sender_email: String,
    pub receiver_email: String,
    in_reply_to: Option<String>,
}

impl ParsedEmail {
    pub fn new(
        subject: Option<String>,
        body: String,
        sender_email: String,
        receiver_email: String,
        in_reply_to: Option<String>,
    ) -> Self {
        ParsedEmail {
            subject,
            body,
            sender_email,
            receiver_email,
            in_reply_to,
        }
    }

    pub fn message_id(&self) -> Option<String> {
        let target = self.in_reply_to.clone()?;
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

pub fn parse_email(email_bytes: &Bytes) -> Result<ParsedEmail, String> {
    let message = MessageParser::default()
        .parse(&email_bytes)
        .ok_or("Failed to parse email")?;
    let subject = message.subject();
    let body = message
        .body_text(0)
        .ok_or("Failed to parse email body")?
        .into_owned();
    let reply_body = EmailReplyParser::parse_reply(&body);
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
        subject.map(|s| s.to_string()),
        reply_body,
        sender_email,
        receiver_email,
        in_reply_to,
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
        let message_id = parsed_email.message_id();
        let correct_message_id =
            Some("010f019ab18dd4f1-e4d8dbab-6e05-466a-9cdb-5c9ccde5f3de-000000".to_string());
        assert_eq!(message_id, correct_message_id);
    }
}
