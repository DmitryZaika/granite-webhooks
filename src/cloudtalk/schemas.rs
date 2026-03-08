use serde::{Deserialize, Deserializer, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct CloudtalkSMS {
    id: Option<i32>,
    sender: CleanedPhone,
    recipient: CleanedPhone,
    text: CleanText,
    agent: Option<String>,
}

impl CloudtalkSMS {
    pub const fn sender(&self) -> u64 {
        self.sender.0
    }
    pub const fn receipent(&self) -> u64 {
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
        assert_eq!(sms.receipent(), 3173161456);

        assert_eq!(sms.text.0, "Не пиши сюда");
        assert!(!sms.text.0.contains("[text]"));

        assert_eq!(sms.agent, Some("540273".to_string()));
    }

    #[test]
    #[should_panic]
    fn test_cloudtalk_bare_payload_parsing() {
        let sms: CloudtalkSMS = serde_json::from_slice(MESSAGE_2).expect("Failed to parse JSON");

        assert_eq!(sms.sender(), 6468956758);
        assert_eq!(sms.receipent(), 3173161456);

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
