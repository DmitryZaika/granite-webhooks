use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct RingcentralSMS {
    pub id: Option<i64>,
    sender: CleanedPhone,
    recipient: CleanedPhone,
    pub text: CleanText,
    pub agent: Option<String>,
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

const OUTBOUND_FOLLOWUP_MIN_TALKING_SECONDS: u64 = 60;

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
        serde_json::Value::Bool(_) | serde_json::Value::Null | serde_json::Value::Array(_)
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
pub fn outbound_call_followup_check(value: &serde_json::Value) -> Option<OutboundCallFollowupCheck> {
    if !call_payload_is_outgoing(value) {
        return None;
    }
    let talking_time = first_field_in_call_objects(value, &["talking_time", "talkingTime"])
        .and_then(json_u64)?;
    if talking_time <= OUTBOUND_FOLLOWUP_MIN_TALKING_SECONDS {
        return None;
    }
    let phone_digits = customer_phone_from_call_payload(value)?;
    let call_id = first_field_in_call_objects(value, &["id", "call_id", "callId"]).and_then(json_u64)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::ringcentral::{INBOUND_NULL_TEXT, INBOUND_SMS};
    use serde_json;

    const MESSAGE_2: &[u8] =
        b"{\"id\":null,\"from\":\"[sender]\",\"to\":\"[recipient]\",\"body\":\"[text]\"}";

    #[test]
    fn test_ringcentral_payload_parsing() {
        let sms: RingcentralSMS = serde_json::from_slice(INBOUND_SMS).expect("Failed to parse JSON");

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
        let outgoing: serde_json::Value = serde_json::from_str(
            r#"{"external_number":"+15551234567","type":"outgoing"}"#,
        )
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
    fn outbound_call_followup_check_requires_outgoing_over_60s() {
        let short: serde_json::Value = serde_json::from_str(
            r#"{"Cdr":{"public_external":"+15551234567","type":"outgoing","talking_time":"60","id":"99"}}"#,
        )
        .unwrap();
        assert_eq!(outbound_call_followup_check(&short), None);

        let long: serde_json::Value = serde_json::from_str(
            r#"{"Cdr":{"public_external":"+15551234567","type":"outgoing","talking_time":"61","id":"77","is_voicemail":true}}"#,
        )
        .unwrap();
        assert_eq!(
            outbound_call_followup_check(&long),
            Some(OutboundCallFollowupCheck {
                phone_digits: 5_551_234_567,
                call_id: 77,
                talking_time: 61,
                is_voicemail: true,
                recording_link: None,
            })
        );
    }
}
