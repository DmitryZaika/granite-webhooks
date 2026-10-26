//! Telnyx messaging: sending a text (`POST /messages`), downloading the
//! files of an inbound MMS, and the events of the messaging webhook
//! (`message.received`, `message.sent`, `message.finalized`).
//!
//! Nothing here logs phone numbers, message text, media links or Telnyx's
//! error details.

use super::{DOWNLOAD_TIMEOUT, Telnyx, TelnyxError};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};

/// A text to send through the company's messaging profile.
#[derive(Debug, Clone, Copy)]
pub struct OutgoingMessage<'a> {
    /// Our line, E.164.
    pub from: &'a str,
    /// The customer, E.164.
    pub to: &'a str,
    /// The text; may be empty with media.
    pub text: &'a str,
    /// Links Telnyx fetches the files from (an MMS); empty for an SMS.
    pub media_urls: &'a [String],
    pub messaging_profile_id: &'a str,
    /// Where Telnyx reports how the text went (`message.sent`,
    /// `message.finalized`).
    pub webhook_url: &'a str,
}

/// Telnyx took the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    /// Telnyx's message id (a UUID).
    pub id: String,
    /// Its first recipient's status (`queued` at first).
    pub status: Option<String>,
}

impl Telnyx {
    /// Sends a text (`POST /messages`). A refusal is
    /// [`TelnyxError::Status`] with Telnyx's code (`40300`: the customer
    /// texted STOP).
    pub async fn send_message(
        &self,
        message: &OutgoingMessage<'_>,
    ) -> Result<SentMessage, TelnyxError> {
        let mut body = json!({
            "from": message.from,
            "to": message.to,
            "messaging_profile_id": message.messaging_profile_id,
            "webhook_url": message.webhook_url,
        });
        if !message.text.is_empty() {
            body["text"] = json!(message.text);
        }
        if !message.media_urls.is_empty() {
            body["media_urls"] = json!(message.media_urls);
        }
        let answer = self.json(Method::POST, "/messages", Some(&body)).await?;
        let data = &answer["data"];
        let id = data["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(TelnyxError::Unexpected("message without id"))?;
        Ok(SentMessage {
            id: id.to_string(),
            status: data["to"][0]["status"].as_str().map(str::to_string),
        })
    }

    /// Downloads a file of an inbound MMS from the link Telnyx's webhook
    /// gave: its bytes and type. The link is tried as it is first; only when
    /// Telnyx's media host answers 401/403 is it tried again with our key
    /// (never sent to another host). Refused when over `max_bytes` or empty.
    pub async fn download_media(
        &self,
        url: &str,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, Option<String>), TelnyxError> {
        let key = self.key.as_deref().ok_or(TelnyxError::NotConfigured)?;
        let parsed = url::Url::parse(url).map_err(|_| TelnyxError::Unexpected("bad media link"))?;
        if !matches!(parsed.scheme(), "https" | "http") {
            return Err(TelnyxError::Unexpected("media link is not HTTP"));
        }
        let get = |authorized: bool| {
            let request = self.http.get(url).timeout(DOWNLOAD_TIMEOUT);
            if authorized {
                request.bearer_auth(key)
            } else {
                request
            }
        };
        let mut response = get(false)
            .send()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        if matches!(response.status().as_u16(), 401 | 403) && self.trusts_media_host(&parsed) {
            response = get(true)
                .send()
                .await
                .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        }
        if !response.status().is_success() {
            return Err(TelnyxError::Status {
                status: response.status().as_u16(),
                code: None,
            });
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
            })
            .filter(|value| !value.is_empty());
        if response
            .content_length()
            .is_some_and(|length| length > u64::try_from(max_bytes).unwrap_or(u64::MAX))
        {
            return Err(TelnyxError::Unexpected("media too large"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?
        {
            if bytes.len() + chunk.len() > max_bytes {
                return Err(TelnyxError::Unexpected("media too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Err(TelnyxError::Unexpected("empty media"));
        }
        Ok((bytes, content_type))
    }

    /// Telnyx's own hosts (and the API root's, a stand-in in tests): the
    /// only ones our key may go to.
    fn trusts_media_host(&self, url: &url::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let api_host = url::Url::parse(&self.base)
            .ok()
            .and_then(|base| base.host_str().map(str::to_string));
        host == "telnyx.com" || host.ends_with(".telnyx.com") || api_host.as_deref() == Some(host)
    }
}

/// A messaging webhook as Telnyx posts it (`{"data": {...}, "meta": {...}}`);
/// fields we do not read are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct MessagingWebhook {
    pub data: MessagingEvent,
}

/// One messaging event.
#[derive(Debug, Clone, Deserialize)]
pub struct MessagingEvent {
    /// `message.received`, `message.sent` or `message.finalized`.
    pub event_type: String,
    /// The event's id (repeats of the event carry the same one).
    #[serde(default)]
    pub id: Option<String>,
    /// When it happened (RFC 3339).
    #[serde(default)]
    pub occurred_at: Option<String>,
    #[serde(default)]
    pub payload: MessagePayload,
}

/// The message an event is about.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MessagePayload {
    /// Telnyx's message id (a UUID).
    pub id: Option<String>,
    /// `inbound` or `outbound`.
    pub direction: Option<String>,
    pub from: Option<Party>,
    pub to: Vec<Party>,
    /// The text; null for a media-only MMS.
    pub text: Option<String>,
    /// The files of an MMS.
    pub media: Vec<Media>,
    /// Why a text failed.
    pub errors: Vec<MessageError>,
    pub messaging_profile_id: Option<String>,
}

/// A sender or recipient.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Party {
    /// E.164.
    pub phone_number: Option<String>,
    /// A recipient's delivery status: `queued`, `sending`, `sent`,
    /// `delivered`, `sending_failed`, `delivery_failed`,
    /// `delivery_unconfirmed` (`webhook_delivered` on inbound texts).
    pub status: Option<String>,
}

/// A file of an inbound MMS, still on Telnyx (the link expires).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Media {
    pub url: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
}

/// One of a failed text's errors.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MessageError {
    /// Telnyx's code, e.g. `40300` (sent as text or as a number).
    pub code: Value,
}

impl MessageError {
    /// The code as text.
    pub fn code(&self) -> Option<String> {
        match &self.code {
            Value::String(code) => Some(code.trim().to_string()).filter(|code| !code.is_empty()),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        }
    }
}

impl MessagePayload {
    /// The first recipient's delivery status.
    pub fn status(&self) -> Option<&str> {
        self.to.first().and_then(|to| to.status.as_deref())
    }

    /// The first error's code.
    pub fn error_code(&self) -> Option<String> {
        self.errors.iter().find_map(MessageError::code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhooks_are_read_with_unknown_fields_ignored() {
        let hook: MessagingWebhook = serde_json::from_value(json!({
            "data": { "event_type": "message.finalized", "id": "ev-1", "record_type": "event",
              "payload": { "id": "msg-1", "direction": "outbound", "cost": { "amount": "0.0051" },
                "from": { "phone_number": "+13175550101" },
                "to": [{ "phone_number": "+13175550142", "status": "delivery_failed" }],
                "errors": [{ "code": 40300, "title": "Blocked" }], "text": null } },
            "meta": { "attempt": 1 }
        }))
        .unwrap();
        assert_eq!(hook.data.event_type, "message.finalized");
        assert_eq!(hook.data.payload.status(), Some("delivery_failed"));
        assert_eq!(hook.data.payload.error_code().as_deref(), Some("40300"));
        assert_eq!(hook.data.payload.text, None);
        let text_code = MessageError {
            code: json!("40001"),
        };
        assert_eq!(text_code.code().as_deref(), Some("40001"));
    }

    #[test]
    fn our_key_only_goes_to_telnyx_hosts() {
        let telnyx = Telnyx::new(Some("k"), "http://127.0.0.1:9/telnyx");
        let trusts = |url: &str| telnyx.trusts_media_host(&url::Url::parse(url).unwrap());
        assert!(trusts("https://media.telnyx.com/a.jpg"));
        assert!(trusts("http://127.0.0.1:9/media/a.jpg"));
        assert!(!trusts("https://media.telnyx.com.evil.test/a.jpg"));
        assert!(!trusts("https://s3.amazonaws.com/a.jpg"));
    }
}
