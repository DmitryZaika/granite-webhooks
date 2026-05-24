use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct CloudtalkSMS {
    pub id: Option<i32>,
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
        let raw_s = String::deserialize(deserializer)?;

        // Remove the "[text]" prefix if it exists
        let cleaned = raw_s.replacen("[text]", "", 1);

        Ok(Self(cleaned))
    }
}

fn get_last_n_chars(s: &str, n: usize) -> &str {
    // Find the byte index of the Nth character from the end.
    // char_indices() provides the byte index and the character.
    let byte_index = s
        .char_indices()
        .rev() // Reverse the iterator to start from the end
        .nth(n - 1) // Get the Nth character's entry
        .map(|(i, _)| i)
        .unwrap(); // Extract only the byte index

    &s[byte_index..]
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

#[derive(Debug, Default, Serialize)]
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

#[derive(Serialize, Debug, Clone)]
pub struct ContactNumber {
    pub public_number: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct ContactEmail {
    pub email: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct ExternalUrl {
    pub name: String,
    pub url: String,
}

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
    use serde_json;

    const MESSAGE_1: &[u8] = b"{\"id\":null,\"sender\":\"+16468956758[sender]\",\"recipient\":\"+13173161456[recipient]\",\"text\":\"[text]\xd0\x9d\xd0\xb5 \xd0\xbf\xd0\xb8\xd1\x88\xd0\xb8 \xd1\x81\xd1\x8e\xd0\xb4\xd0\xb0\",\"agent\":\"540273\"}";
    const MESSAGE_2: &[u8] =
        b"{\"id\":null,\"from\":\"[sender]\",\"to\":\"[recipient]\",\"body\":\"[text]\"}";

    #[test]
    fn test_cloudtalk_payload_parsing() {
        let sms: CloudtalkSMS = serde_json::from_slice(MESSAGE_1).expect("Failed to parse JSON");

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
    fn test_phone_without_prefix() {
        // Test that it still works if the '1' isn't there
        let json = r#""5551234567""#;
        let phone: CleanedPhone = serde_json::from_str(json).unwrap();
        assert_eq!(phone.0, 5551234567);
    }
}
