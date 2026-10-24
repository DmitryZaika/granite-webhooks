use crate::crud::ringcentral::CustomerWithMapping;
use crate::ringcentral::api::PublicRcContactPayload;
use regex::Regex;
use std::sync::LazyLock;

/// Longest name part sent. `RingCentral` refuses a contact whose name is too
/// long (it took 42 characters and refused 66); names that long are notes
/// typed into the name field.
const MAX_NAME_CHARS: usize = 40;

/// An email `RingCentral` takes. It refuses the whole contact over a
/// malformed one (a phone number, a stray `>`, letters typed in Cyrillic).
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}$").unwrap()
});

pub fn phone_digits_only(phone: &str) -> String {
    phone.chars().filter(char::is_ascii_digit).collect()
}

pub fn normalize_to_e164(phone: Option<&str>) -> Option<String> {
    let trimmed = phone?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('+') {
        return Some(format!("+{}", phone_digits_only(trimmed)));
    }
    let digits = phone_digits_only(trimmed);
    if digits.len() == 10 {
        return Some(format!("+1{digits}"));
    }
    if digits.len() == 11 && digits.starts_with('1') {
        return Some(format!("+{digits}"));
    }
    None
}

pub fn split_name(name: Option<&str>) -> (String, String) {
    let trimmed = name.unwrap_or("").trim();
    if trimmed.is_empty() {
        return ("Customer".to_string(), String::new());
    }
    let mut parts = trimmed.split_whitespace();
    let first = clip_name(parts.next().unwrap_or("Customer"));
    let last = clip_name(&parts.collect::<Vec<_>>().join(" "));
    (first, last)
}

fn clip_name(part: &str) -> String {
    let clipped: String = part.chars().take(MAX_NAME_CHARS).collect();
    clipped.trim_end().to_string()
}

pub fn build_rc_contact_payload(
    customer: &CustomerWithMapping,
    first_name: String,
    last_name: String,
) -> PublicRcContactPayload {
    let phones: Vec<String> = [&customer.phone, &customer.phone_2]
        .iter()
        .filter_map(|p| normalize_to_e164(p.as_deref()))
        .collect();
    let email = customer
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| EMAIL_RE.is_match(s))
        .map(str::to_string);
    let notes = std::env::var("APP_URL").ok().and_then(|app_url| {
        if app_url.is_empty() {
            None
        } else {
            Some(format!(
                "Granite Manager: {app_url}/employee/customers/info/{}/info",
                customer.id
            ))
        }
    });

    PublicRcContactPayload {
        first_name,
        last_name,
        email,
        mobile_phone: phones.first().cloned(),
        home_phone: phones.get(1).cloned(),
        business_phone: None,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::{EMAIL_RE, normalize_to_e164, phone_digits_only, split_name};

    #[test]
    fn phone_digits_only_strips_formatting() {
        assert_eq!(phone_digits_only("+1 (555) 123-4567"), "15551234567");
    }

    #[test]
    fn normalize_to_e164_accepts_us_numbers() {
        assert_eq!(
            normalize_to_e164(Some("5551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(
            normalize_to_e164(Some("15551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(
            normalize_to_e164(Some("+15551234567")),
            Some("+15551234567".to_string())
        );
        assert_eq!(normalize_to_e164(Some("123")), None);
        assert_eq!(normalize_to_e164(Some("")), None);
        assert_eq!(normalize_to_e164(None), None);
    }

    #[test]
    fn split_name_keeps_a_usable_first_name() {
        assert_eq!(
            split_name(Some("Pat Stone")),
            ("Pat".to_string(), "Stone".to_string())
        );
        assert_eq!(
            split_name(Some("  ")),
            ("Customer".to_string(), String::new())
        );
        assert_eq!(split_name(None), ("Customer".to_string(), String::new()));
    }

    #[test]
    fn split_name_clips_a_note_typed_into_the_name() {
        let (first, last) = split_name(Some(&format!("Pat {}", "word ".repeat(30))));
        assert_eq!(first, "Pat");
        assert_eq!(last.chars().count(), 39);
        assert!(!last.ends_with(' '));
    }

    #[test]
    fn only_well_formed_emails_are_sent() {
        for good in ["pat@example.com", "pat.stone+quotes@mail.example.co"] {
            assert!(EMAIL_RE.is_match(good), "{good}");
        }
        for bad in [
            "317-555-0100",
            "pat@example.com>",
            "pat@#example.com",
            "pat\u{44b}@example.com",
            "pat@example",
            "pat stone@example.com",
        ] {
            assert!(!EMAIL_RE.is_match(bad), "{bad}");
        }
    }
}
