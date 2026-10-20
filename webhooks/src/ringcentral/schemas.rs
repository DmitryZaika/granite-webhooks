use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct RingcentralSMS {
    pub id: Option<i64>,
    sender: CleanedPhone,
    recipient: CleanedPhone,
    pub text: CleanText,
    pub agent: Option<String>,
    /// Pictures and files of an MMS; only `RingCentral`'s own push carries them.
    #[serde(skip)]
    pub media: Vec<MmsMedia>,
}

/// One file of an inbound MMS, still on `RingCentral`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MmsMedia {
    /// The attachment id under the message's `content/`.
    pub id: String,
    pub content_type: String,
}

impl RingcentralSMS {
    pub const fn sender(&self) -> u64 {
        self.sender.0
    }
    pub const fn recipient(&self) -> u64 {
        self.recipient.0
    }
}

#[derive(Serialize, Debug)]
pub struct CleanedPhone(u64);

impl<'de> Deserialize<'de> for CleanedPhone {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // 1. Get the raw string from the JSON
        let raw_s = String::deserialize(deserializer)?;

        // 2. Clean the string: keep only digits
        let cleaned: String = raw_s.chars().filter(char::is_ascii_digit).collect();

        let stripped = get_last_n_chars(&cleaned, 10);

        // 3. Parse to i64 (and handle errors if the string is empty/invalid)
        let num = stripped.parse::<u64>().map_err(serde::de::Error::custom)?;

        Ok(Self(num))
    }
}

// --- Text Cleaning Type ---
#[derive(Serialize, Debug)]
pub struct CleanText(pub String);

impl<'de> Deserialize<'de> for CleanText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // RingCentral may send null text for an empty body; treat as empty.
        let raw_s = Option::<String>::deserialize(deserializer)?.unwrap_or_default();

        // Remove the "[text]" prefix if it exists
        let cleaned = raw_s.replacen("[text]", "", 1);

        Ok(Self(cleaned))
    }
}

fn get_last_n_chars(s: &str, n: usize) -> &str {
    // Return the whole string if it has fewer than n chars, instead of panicking.
    match s.char_indices().rev().nth(n - 1).map(|(i, _)| i) {
        Some(byte_index) => &s[byte_index..],
        None => s,
    }
}

/// Last 10 digit characters as a number. `None` when there are fewer than 10 digits
/// so a CAST of 0 cannot false-match an enrollment phone.
pub fn phone_last10(raw: &str) -> Option<u64> {
    let cleaned: String = raw.chars().filter(char::is_ascii_digit).collect();
    if cleaned.len() < 10 {
        return None;
    }
    get_last_n_chars(&cleaned, 10).parse().ok()
}

fn json_phone_raw(value: &serde_json::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    value.as_u64().map(|n| n.to_string())
}

fn first_phone_in_object(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    const PHONE_KEYS: [&str; 6] = [
        "external_number",
        "public_external",
        "caller",
        "calling",
        "from",
        "number",
    ];
    for key in PHONE_KEYS {
        if let Some(raw) = obj.get(key).and_then(json_phone_raw)
            && let Some(digits) = phone_last10(&raw)
        {
            return Some(digits);
        }
    }
    None
}

fn nested_object<'a>(
    value: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    value.get(key).and_then(serde_json::Value::as_object)
}

fn collect_call_type_strings(value: &serde_json::Value) -> Vec<String> {
    const TYPE_KEYS: [&str; 3] = ["type", "direction", "call_type"];
    let mut types = Vec::new();
    let mut push_from = |obj: &serde_json::Map<String, serde_json::Value>| {
        for key in TYPE_KEYS {
            if let Some(raw) = obj.get(key).and_then(json_phone_raw) {
                types.push(raw);
            }
        }
    };
    if let Some(obj) = value.as_object() {
        push_from(obj);
    }
    for nested in ["properties", "Cdr", "cdr", "call"] {
        if let Some(obj) = nested_object(value, nested) {
            push_from(obj);
        }
    }
    types
}

fn call_payload_is_outgoing_or_internal(value: &serde_json::Value) -> bool {
    collect_call_type_strings(value).iter().any(|raw| {
        let lower = raw.to_ascii_lowercase();
        lower.contains("out") || lower == "internal"
    })
}

fn call_payload_is_outgoing(value: &serde_json::Value) -> bool {
    collect_call_type_strings(value).iter().any(|raw| {
        let lower = raw.to_ascii_lowercase();
        lower.contains("out")
    })
}

fn customer_phone_from_call_payload(value: &serde_json::Value) -> Option<u64> {
    if let Some(obj) = value.as_object()
        && let Some(digits) = first_phone_in_object(obj)
    {
        return Some(digits);
    }
    for nested in ["properties", "Cdr", "cdr", "call"] {
        if let Some(obj) = nested_object(value, nested)
            && let Some(digits) = first_phone_in_object(obj)
        {
            return Some(digits);
        }
    }
    None
}

/// Customer phone last-10 from a RingCentral call webhook, or `None` when the
/// payload is outgoing/internal or has no usable number. Missing type is treated
/// as inbound so an incoming-only workflow still cancels.
pub fn inbound_customer_phone_from_call_payload(value: &serde_json::Value) -> Option<u64> {
    if call_payload_is_outgoing_or_internal(value) {
        return None;
    }
    customer_phone_from_call_payload(value)
}

/// Shorter outbound calls are not worth transcribing.
const OUTBOUND_FOLLOWUP_MIN_TALKING_SECONDS: u64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundCallFollowupCheck {
    pub phone_digits: u64,
    pub call_id: u64,
    pub talking_time: u64,
    pub is_voicemail: bool,
    pub recording_link: Option<String>,
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Bool(_)
        | serde_json::Value::Null
        | serde_json::Value::Array(_)
        | serde_json::Value::Object(_) => None,
    }
}

fn json_bool(value: &serde_json::Value) -> Option<bool> {
    match value {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => n.as_u64().map(|v| v != 0),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn first_field_in_call_objects<'a>(
    value: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    let visit = |obj: &'a serde_json::Map<String, serde_json::Value>| {
        for key in keys {
            if let Some(v) = obj.get(*key) {
                return Some(v);
            }
        }
        None
    };
    if let Some(obj) = value.as_object()
        && let Some(v) = visit(obj)
    {
        return Some(v);
    }
    for nested in ["properties", "Cdr", "cdr", "call"] {
        if let Some(obj) = nested_object(value, nested)
            && let Some(v) = visit(obj)
        {
            return Some(v);
        }
    }
    None
}

/// Outbound call longer than 60s talking time — enqueue background transcript
/// check before cancelling automated follow-ups.
pub fn outbound_call_followup_check(
    value: &serde_json::Value,
) -> Option<OutboundCallFollowupCheck> {
    if !call_payload_is_outgoing(value) {
        return None;
    }
    let talking_time =
        first_field_in_call_objects(value, &["talking_time", "talkingTime"]).and_then(json_u64)?;
    if talking_time <= OUTBOUND_FOLLOWUP_MIN_TALKING_SECONDS {
        return None;
    }
    let phone_digits = customer_phone_from_call_payload(value)?;
    let call_id =
        first_field_in_call_objects(value, &["id", "call_id", "callId"]).and_then(json_u64)?;
    let is_voicemail = first_field_in_call_objects(value, &["is_voicemail", "isVoicemail"])
        .and_then(json_bool)
        .unwrap_or(false);
    let recording_link = first_field_in_call_objects(value, &["recording_link", "recordingLink"])
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Some(OutboundCallFollowupCheck {
        phone_digits,
        call_id,
        talking_time,
        is_voicemail,
        recording_link,
    })
}

/// `RingCentral`'s own push for a `message-store/instant?type=SMS` subscription
/// (the message itself, inbound only) or a `message-store` one (only what
/// changed; how texts sent from the `RingCentral` app reach us).
#[derive(Deserialize, Debug)]
pub struct RingcentralSmsNotification {
    #[serde(default)]
    event: String,
    body: Option<NotificationMessage>,
}

#[derive(Deserialize, Debug)]
struct NotificationMessage {
    id: Option<serde_json::Value>,
    direction: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    from: Option<NotificationParty>,
    #[serde(default)]
    to: Vec<NotificationParty>,
    subject: Option<String>,
    #[serde(default)]
    attachments: Vec<NotificationAttachment>,
    /// A `message-store` push: the kinds of message that changed.
    #[serde(default)]
    changes: Vec<MessageStoreChange>,
}

#[derive(Deserialize, Debug)]
struct MessageStoreChange {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "newCount", default)]
    new_count: i64,
}

#[derive(Deserialize, Debug)]
struct NotificationAttachment {
    id: Option<serde_json::Value>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "contentType")]
    content_type: Option<String>,
}

impl NotificationAttachment {
    /// A picture or file the customer sent; the text part is the subject already.
    fn into_media(self) -> Option<MmsMedia> {
        if self.kind.as_deref() != Some("MmsAttachment") {
            return None;
        }
        let id = match self.id? {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) if !s.is_empty() => s,
            _ => return None,
        };
        Some(MmsMedia {
            id,
            content_type: self
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
        })
    }
}

#[derive(Deserialize, Debug)]
struct NotificationParty {
    #[serde(rename = "phoneNumber")]
    phone_number: Option<String>,
}

#[derive(Debug)]
pub enum NotifiedSms {
    Inbound(RingcentralSMS),
    Outbound(RingcentralSMS),
}

impl RingcentralSmsNotification {
    /// The extension of a `message-store` push that has new texts.
    pub fn new_sms_extension(&self) -> Option<String> {
        if !self.event.contains("/message-store") || self.event.contains("/instant") {
            return None;
        }
        let new_texts = self.body.as_ref()?.changes.iter().any(|change| {
            change.kind.as_deref() == Some("SMS") && change.new_count > 0
        });
        new_texts.then(|| extension_from_event(&self.event)).flatten()
    }

    /// One `message-store` record of `extension`, as its push would carry it.
    pub fn from_record(extension: &str, record: serde_json::Value) -> Option<NotifiedSms> {
        Self {
            event: format!("/restapi/v1.0/account/~/extension/{extension}/message-store"),
            body: serde_json::from_value(record).ok(),
        }
        .into_sms()
    }

    /// The SMS in the same shape the `/ringcentral/sms` routes store. `None` for anything
    /// that is not a one-to-one SMS with both phone numbers.
    pub fn into_sms(self) -> Option<NotifiedSms> {
        let body = self.body?;
        if body.kind.as_deref() != Some("SMS") {
            return None;
        }
        let sender = phone_last10(body.from?.phone_number.as_deref()?)?;
        let recipient = phone_last10(body.to.first()?.phone_number.as_deref()?)?;
        let id = match body.id {
            Some(serde_json::Value::Number(n)) => n.as_i64(),
            Some(serde_json::Value::String(s)) => s.parse().ok(),
            _ => None,
        };
        let sms = RingcentralSMS {
            id,
            sender: CleanedPhone(sender),
            recipient: CleanedPhone(recipient),
            text: CleanText(body.subject.unwrap_or_default()),
            agent: extension_from_event(&self.event),
            media: body
                .attachments
                .into_iter()
                .filter_map(NotificationAttachment::into_media)
                .collect(),
        };
        match body.direction.as_deref() {
            Some("Inbound") => Some(NotifiedSms::Inbound(sms)),
            Some("Outbound") => Some(NotifiedSms::Outbound(sms)),
            _ => None,
        }
    }
}

/// `/restapi/v1.0/account/~/extension/540273/message-store/instant?type=SMS` → `540273`.
fn extension_from_event(event: &str) -> Option<String> {
    let rest = event.split("/extension/").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::ringcentral::{INBOUND_NULL_TEXT, INBOUND_SMS};
    use serde_json;

    const MESSAGE_2: &[u8] =
        b"{\"id\":null,\"from\":\"[sender]\",\"to\":\"[recipient]\",\"body\":\"[text]\"}";

    #[test]
    fn test_ringcentral_payload_parsing() {
        let sms: RingcentralSMS =
            serde_json::from_slice(INBOUND_SMS).expect("Failed to parse JSON");

        assert_eq!(sms.sender(), 6468956758);
        assert_eq!(sms.recipient(), 3173161456);

        assert_eq!(sms.text.0, "Не пиши сюда");
        assert!(!sms.text.0.contains("[text]"));

        assert_eq!(sms.agent, Some("540273".to_string()));
    }

    #[test]
    #[should_panic]
    fn test_ringcentral_bare_payload_parsing() {
        let sms: RingcentralSMS = serde_json::from_slice(MESSAGE_2).expect("Failed to parse JSON");

        assert_eq!(sms.sender(), 6468956758);
        assert_eq!(sms.recipient(), 3173161456);

        assert_eq!(sms.text.0, "Не пиши сюда");
        assert!(!sms.text.0.contains("[text]"));

        assert_eq!(sms.agent, Some("540273".to_string()));
    }

    #[test]
    fn test_null_text_parses_as_empty_string() {
        let sms: RingcentralSMS =
            serde_json::from_slice(INBOUND_NULL_TEXT).expect("null text must parse");

        assert_eq!(sms.text.0, "");
        assert_eq!(sms.id, Some(51753924));
        assert_eq!(sms.sender(), 6468956758);
    }

    #[test]
    fn test_phone_without_prefix() {
        // Test that it still works if the '1' isn't there
        let json = r#""5551234567""#;
        let phone: CleanedPhone = serde_json::from_str(json).unwrap();
        assert_eq!(phone.0, 5551234567);
    }

    #[test]
    fn phone_last10_requires_ten_digits() {
        assert_eq!(phone_last10("+1 (555) 123-4567"), Some(5_551_234_567));
        assert_eq!(phone_last10("5551234567"), Some(5_551_234_567));
        assert_eq!(phone_last10("555-1234"), None);
        assert_eq!(phone_last10(""), None);
    }

    #[test]
    fn inbound_call_payload_reads_common_phone_keys() {
        let top: serde_json::Value =
            serde_json::from_str(r#"{"external_number":"+15551234567","type":"incoming"}"#)
                .unwrap();
        assert_eq!(
            inbound_customer_phone_from_call_payload(&top),
            Some(5_551_234_567)
        );

        let nested: serde_json::Value = serde_json::from_str(
            r#"{"properties":{"external_number":"5551234567"},"Cdr":{"type":"incoming"}}"#,
        )
        .unwrap();
        assert_eq!(
            inbound_customer_phone_from_call_payload(&nested),
            Some(5_551_234_567)
        );

        let cdr: serde_json::Value =
            serde_json::from_str(r#"{"Cdr":{"public_external":"+15551234567","type":"incoming"}}"#)
                .unwrap();
        assert_eq!(
            inbound_customer_phone_from_call_payload(&cdr),
            Some(5_551_234_567)
        );
    }

    #[test]
    fn outbound_call_payload_does_not_yield_a_phone() {
        let outgoing: serde_json::Value =
            serde_json::from_str(r#"{"external_number":"+15551234567","type":"outgoing"}"#)
                .unwrap();
        assert_eq!(inbound_customer_phone_from_call_payload(&outgoing), None);

        let missing_type: serde_json::Value =
            serde_json::from_str(r#"{"external_number":"+15551234567"}"#).unwrap();
        assert_eq!(
            inbound_customer_phone_from_call_payload(&missing_type),
            Some(5_551_234_567)
        );
    }

    #[test]
    fn outbound_call_followup_check_requires_outgoing_over_30s() {
        let short: serde_json::Value = serde_json::from_str(
            r#"{"Cdr":{"public_external":"+15551234567","type":"outgoing","talking_time":"30","id":"99"}}"#,
        )
        .unwrap();
        assert_eq!(outbound_call_followup_check(&short), None);

        let long: serde_json::Value = serde_json::from_str(
            r#"{"Cdr":{"public_external":"+15551234567","type":"outgoing","talking_time":"31","id":"77","is_voicemail":true}}"#,
        )
        .unwrap();
        assert_eq!(
            outbound_call_followup_check(&long),
            Some(OutboundCallFollowupCheck {
                phone_digits: 5_551_234_567,
                call_id: 77,
                talking_time: 31,
                is_voicemail: true,
                recording_link: None,
            })
        );
    }

    fn notification(json: &str) -> RingcentralSmsNotification {
        serde_json::from_str(json).expect("notification json")
    }

    #[test]
    fn notification_inbound_sms_maps_to_stored_shape() {
        let parsed: RingcentralSmsNotification =
            serde_json::from_slice(crate::tests::ringcentral::NOTIFY_INBOUND_SMS).unwrap();
        let Some(NotifiedSms::Inbound(sms)) = parsed.into_sms() else {
            panic!("expected inbound sms");
        };
        assert_eq!(sms.id, Some(3_894_319_559_027));
        assert_eq!(sms.sender(), 6_468_956_758);
        assert_eq!(sms.recipient(), 3_173_161_456);
        assert_eq!(sms.text.0, "Is the slab still available?");
        assert_eq!(sms.agent.as_deref(), Some("540273"));
        assert!(sms.media.is_empty());
    }

    #[test]
    fn notification_keeps_mms_pictures() {
        let mms = notification(
            r#"{"event":"/restapi/v1.0/account/2765489027/extension/2765489027/message-store/instant?type=SMS","body":{"id":"3912429357027","type":"SMS","direction":"Inbound","from":{"phoneNumber":"+13179384645"},"to":[{"phoneNumber":"+13176767938"}],"subject":" ","attachments":[{"id":"3912429357027","type":"Text","contentType":"text/plain"},{"id":"445048423027","type":"MmsAttachment","uri":"https://media.ringcentral.com/x","contentType":"image/png","size":775135}]}}"#,
        );
        let Some(NotifiedSms::Inbound(sms)) = mms.into_sms() else {
            panic!("expected inbound sms");
        };
        assert_eq!(
            sms.media,
            vec![MmsMedia {
                id: "445048423027".into(),
                content_type: "image/png".into(),
            }]
        );
    }

    #[test]
    fn notification_outbound_and_skips() {
        let outbound = notification(
            r#"{"event":"/restapi/v1.0/account/~/extension/~/message-store/instant?type=SMS","body":{"id":5,"type":"SMS","direction":"Outbound","from":{"phoneNumber":"+13173161456"},"to":[{"phoneNumber":"+16468956758"}],"subject":"Hi"}}"#,
        );
        let Some(NotifiedSms::Outbound(sms)) = outbound.into_sms() else {
            panic!("expected outbound sms");
        };
        assert_eq!(sms.id, Some(5));
        assert_eq!(sms.agent, None);

        let fax = notification(
            r#"{"body":{"type":"Fax","direction":"Inbound","from":{"phoneNumber":"+16468956758"},"to":[{"phoneNumber":"+13173161456"}]}}"#,
        );
        assert!(fax.into_sms().is_none());
        let no_body = notification(r#"{"event":"x"}"#);
        assert!(no_body.into_sms().is_none());
        let no_recipient = notification(
            r#"{"body":{"type":"SMS","direction":"Inbound","from":{"phoneNumber":"+16468956758"},"to":[]}}"#,
        );
        assert!(no_recipient.into_sms().is_none());
    }

    #[test]
    fn message_store_push_names_the_extension_with_new_texts() {
        let parse = |json: &str| -> RingcentralSmsNotification { serde_json::from_str(json).unwrap() };
        let new_sms = parse(
            r#"{"event":"/restapi/v1.0/account/1/extension/2765489027/message-store","body":{"extensionId":2765489027,"changes":[{"type":"SMS","newCount":1,"updatedCount":0}]}}"#,
        );
        assert_eq!(new_sms.new_sms_extension().as_deref(), Some("2765489027"));
        let only_updated = parse(
            r#"{"event":"/restapi/v1.0/account/1/extension/5/message-store","body":{"changes":[{"type":"SMS","newCount":0,"updatedCount":2}]}}"#,
        );
        assert!(only_updated.new_sms_extension().is_none());
        let voicemail = parse(
            r#"{"event":"/restapi/v1.0/account/1/extension/5/message-store","body":{"changes":[{"type":"VoiceMail","newCount":1}]}}"#,
        );
        assert!(voicemail.new_sms_extension().is_none());
        let instant = parse(
            r#"{"event":"/restapi/v1.0/account/~/extension/5/message-store/instant?type=SMS","body":{"id":5,"type":"SMS","direction":"Inbound","from":{"phoneNumber":"+16468956758"},"to":[{"phoneNumber":"+13173161456"}]}}"#,
        );
        assert!(instant.new_sms_extension().is_none());
    }

    #[test]
    fn message_store_record_reads_as_an_outbound_sms() {
        let record = serde_json::json!({
            "id": 3912429357027_i64, "type": "SMS", "direction": "Outbound",
            "from": {"phoneNumber": "+13176767938"}, "to": [{"phoneNumber": "+17652421803"}],
            "subject": "I will be around 9:30am",
        });
        let Some(NotifiedSms::Outbound(sms)) =
            RingcentralSmsNotification::from_record("2765489027", record)
        else {
            panic!("not an outbound sms");
        };
        assert_eq!(sms.id, Some(3912429357027));
        assert_eq!((sms.sender(), sms.recipient()), (3176767938, 7652421803));
        assert_eq!(sms.agent.as_deref(), Some("2765489027"));
        assert_eq!(sms.text.0, "I will be around 9:30am");
    }
}
