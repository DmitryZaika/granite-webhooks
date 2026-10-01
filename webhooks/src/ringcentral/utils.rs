use crate::crud::ringcentral::CustomerWithMapping;
use crate::ringcentral::api::PublicRcContactPayload;

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
    let first = parts.next().unwrap_or("Customer").to_string();
    let last = parts.collect::<Vec<_>>().join(" ");
    (first, last)
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
        .filter(|s| !s.is_empty())
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
    use super::{normalize_to_e164, phone_digits_only, split_name};

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
        assert_eq!(split_name(Some("  ")), ("Customer".to_string(), String::new()));
        assert_eq!(split_name(None), ("Customer".to_string(), String::new()));
    }
}
