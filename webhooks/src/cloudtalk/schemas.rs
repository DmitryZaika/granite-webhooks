use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct CloudtalkSMS {
    pub id: Option<i64>,
    sender: CleanedPhone,
    recipient: CleanedPhone,
    pub text: CleanText,
    pub agent: Option<String>,
}

impl CloudtalkSMS {
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
        // CloudTalk sends null for a caption-less MMS; that is an empty body, not a bad payload.
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

/// Customer phone last-10 from a CloudTalk call webhook, or `None` when the
/// payload is outgoing/internal or has no usable number. Missing type is treated
/// as inbound so an incoming-only workflow still cancels.
pub fn inbound_customer_phone_from_call_payload(value: &serde_json::Value) -> Option<u64> {
    if call_payload_is_outgoing_or_internal(value) {
        return None;
    }
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

#[derive(Deserialize)]
pub struct CloudTalkCountry {
    pub id: Option<serde_json::Value>, // Dynamic type: can be String or Number
    pub iso_code: Option<String>,
    pub iso: Option<String>,
    pub code: Option<String>,
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct CountriesEnvelope {
    #[serde(rename = "responseData")]
    pub response_data: Option<ResponseData>,
}

#[derive(Deserialize)]
pub struct ResponseData {
    pub data: Option<Vec<CountryItem>>,
}

#[derive(Deserialize, Serialize)]
pub struct ResponseDataHits {
    pub data: Option<Vec<ContactSearchHit>>,
}

// Handles the `item.Country ?? item` fallback cleanly
#[derive(Deserialize)]
#[serde(untagged)]
pub enum CountryItem {
    Wrapped {
        #[serde(rename = "Country")]
        country: CloudTalkCountry,
    },
    Direct(CloudTalkCountry),
}

impl CountryItem {
    pub fn into_country(self) -> CloudTalkCountry {
        match self {
            Self::Wrapped { country } | Self::Direct(country) => country,
        }
    }
}

#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ContactPayload {
    pub name: Option<String>,
    #[serde(rename = "ContactNumber")]
    pub contact_number: Vec<ContactNumber>,
    #[serde(rename = "ContactEmail")]
    pub contact_email: Vec<ContactEmail>,
    #[serde(rename = "ExternalUrl")]
    pub external_url: Option<Vec<ExternalUrl>>,
    pub address: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
    pub country_id: Option<u64>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ContactNumber {
    pub public_number: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ContactEmail {
    pub email: String,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ExternalUrl {
    pub name: String,
    pub url: String,
}

#[derive(PartialEq, Eq, Debug, Serialize)]
pub struct ParsedAddress {
    pub street: String,
    pub city: Option<String>,
    pub state: Option<String>,
    pub zip: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContactId {
    Number(i64),
    String(String),
}

impl ContactId {
    /// Coerces the contact ID into a valid, non-zero u64.
    pub fn coerce(&self) -> Option<u64> {
        match self {
            Self::Number(n) => {
                // safely attempt to convert i64 -> u64 (fails if negative)
                let val: u64 = (*n).try_into().ok()?;
                if val > 0 { Some(val) } else { None }
            }
            Self::String(s) => {
                let val = s.parse::<u64>().ok()?;
                if val > 0 { Some(val) } else { None }
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ContactNumberObj {
    pub public_number: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ContactNode {
    pub id: Option<ContactId>,
    pub contact_numbers: Option<Vec<String>>,
    #[serde(rename = "ContactNumber")]
    pub contact_number: Option<Vec<ContactNumberObj>>,
}

#[derive(Serialize, Deserialize)]
pub struct ContactSearchHit {
    #[serde(rename = "Contact")]
    pub contact: Option<ContactNode>,
    pub id: Option<ContactId>,
    pub contact_numbers: Option<Vec<String>>,
    #[serde(rename = "ContactNumber")]
    pub contact_number: Option<Vec<ContactNumberObj>>,
}

/// Handles the flexible `number | string` type from the TypeScript interface.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Id {
    Integer(i64),
    String(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PublicNumber {
    pub public_number: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ContactDetails {
    pub id: Option<Id>,
    pub contact_numbers: Option<Vec<String>>,
    #[serde(rename = "ContactNumber")]
    pub contact_number: Option<Vec<PublicNumber>>,
}

#[derive(Serialize, Deserialize)]
pub struct ContactSearchEnvelope {
    #[serde(rename = "responseData")]
    pub response_data: Option<ResponseDataHits>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::cloudtalk::{INBOUND_MMS_NULL_TEXT, INBOUND_SMS};
    use serde_json;

    const MESSAGE_2: &[u8] =
        b"{\"id\":null,\"from\":\"[sender]\",\"to\":\"[recipient]\",\"body\":\"[text]\"}";

    #[test]
    fn test_cloudtalk_payload_parsing() {
        let sms: CloudtalkSMS = serde_json::from_slice(INBOUND_SMS).expect("Failed to parse JSON");

        assert_eq!(sms.sender(), 6468956758);
        assert_eq!(sms.recipient(), 3173161456);

        assert_eq!(sms.text.0, "Не пиши сюда");
        assert!(!sms.text.0.contains("[text]"));

        assert_eq!(sms.agent, Some("540273".to_string()));
    }

    #[test]
    #[should_panic]
    fn test_cloudtalk_bare_payload_parsing() {
        let sms: CloudtalkSMS = serde_json::from_slice(MESSAGE_2).expect("Failed to parse JSON");

        assert_eq!(sms.sender(), 6468956758);
        assert_eq!(sms.recipient(), 3173161456);

        assert_eq!(sms.text.0, "Не пиши сюда");
        assert!(!sms.text.0.contains("[text]"));

        assert_eq!(sms.agent, Some("540273".to_string()));
    }

    #[test]
    fn test_null_text_parses_as_empty_string() {
        let sms: CloudtalkSMS =
            serde_json::from_slice(INBOUND_MMS_NULL_TEXT).expect("null text must parse");

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
}
