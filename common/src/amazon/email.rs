use aws_config::meta::region::RegionProviderChain;
use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use aws_sdk_sesv2::{Client, Error, config::Region};

pub const DEFAULT_NOREPLY_EMAIL_ADDRESS: &str = "noreply@granite-manager.com";
pub const DEFAULT_SEND_EMAIL_ADDRESS: &str = "sales@granite-manager.com";

pub fn extract_email_address(raw: &str) -> String {
    let trimmed = raw.trim();
    let inner = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open => &trimmed[open + 1..close],
        _ => trimmed,
    };
    inner.trim().to_lowercase()
}

pub fn from_email(company_domain: Option<&str>, user_email: &str) -> String {
    let Some(domain) = company_domain.filter(|value| !value.is_empty()) else {
        return DEFAULT_SEND_EMAIL_ADDRESS.to_string();
    };
    if user_email.contains(domain) {
        return user_email.to_string();
    }
    DEFAULT_SEND_EMAIL_ADDRESS.to_string()
}

pub fn format_sender_from(email_name: Option<&str>, email_address: &str) -> String {
    match email_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("\"{name}\" <{email_address}>"),
        None => email_address.to_string(),
    }
}

pub fn assigned_sender_from(
    company_domain: Option<&str>,
    user_email: Option<&str>,
    email_name: Option<&str>,
) -> String {
    let address = from_email(company_domain, user_email.unwrap_or(""));
    format_sender_from(email_name, &address)
}

pub async fn send_message(to: &[&str], subject: &str, message: &str) -> Result<(), Error> {
    send_message_from(to, subject, message, DEFAULT_NOREPLY_EMAIL_ADDRESS)
        .await
        .map(|_| ())
}

pub async fn send_message_from(
    to: &[&str],
    subject: &str,
    message: &str,
    from: &str,
) -> Result<String, Error> {
    let region_provider = RegionProviderChain::first_try(Region::new("us-east-2"));
    let shared_config = aws_config::from_env().region(region_provider).load().await;
    let client = Client::new(&shared_config);

    let mut dest: Destination = Destination::builder().build();
    dest.to_addresses = Some(to.iter().map(|s| (*s).to_string()).collect());
    let subject_content = Content::builder()
        .data(subject)
        .charset("UTF-8")
        .build()
        .expect("building Content");
    let body_content = Content::builder()
        .data(message)
        .charset("UTF-8")
        .build()
        .expect("building Content");
    let body = Body::builder().html(body_content).build();

    let msg = Message::builder()
        .subject(subject_content)
        .body(body)
        .build();

    let email_content = EmailContent::builder().simple(msg).build();

    let output = client
        .send_email()
        .from_email_address(from)
        .destination(dest)
        .content(email_content)
        .send()
        .await?;

    Ok(output.message_id().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_email_address_unwraps_display_name() {
        assert_eq!(
            extract_email_address("\"Dema Granite Depot\" <dema@granitedepotindy.com>"),
            "dema@granitedepotindy.com"
        );
        assert_eq!(
            extract_email_address("brian@hughesproducts.com"),
            "brian@hughesproducts.com"
        );
    }

    #[test]
    fn from_email_uses_default_when_domain_is_missing() {
        assert_eq!(
            from_email(None, "rep@custom.com"),
            DEFAULT_SEND_EMAIL_ADDRESS
        );
        assert_eq!(
            from_email(Some(""), "rep@custom.com"),
            DEFAULT_SEND_EMAIL_ADDRESS
        );
    }

    #[test]
    fn from_email_uses_user_email_when_it_matches_company_domain() {
        assert_eq!(from_email(Some("acme.com"), "rep@acme.com"), "rep@acme.com");
    }

    #[test]
    fn from_email_falls_back_when_user_email_is_outside_company_domain() {
        assert_eq!(
            from_email(Some("acme.com"), "rep@gmail.com"),
            DEFAULT_SEND_EMAIL_ADDRESS
        );
    }

    #[test]
    fn format_sender_from_includes_display_name() {
        assert_eq!(
            format_sender_from(Some("Alex"), "alex@acme.com"),
            "\"Alex\" <alex@acme.com>"
        );
    }

    #[test]
    fn format_sender_from_returns_bare_address_without_name() {
        assert_eq!(
            format_sender_from(None, "alex@acme.com"),
            "alex@acme.com"
        );
        assert_eq!(
            format_sender_from(Some("  "), "alex@acme.com"),
            "alex@acme.com"
        );
    }

    #[test]
    fn assigned_sender_from_uses_employee_address_and_name() {
        assert_eq!(
            assigned_sender_from(Some("acme.com"), Some("rep@acme.com"), Some("Alex Rep")),
            "\"Alex Rep\" <rep@acme.com>"
        );
    }

    #[test]
    fn assigned_sender_from_falls_back_to_sales_address() {
        assert_eq!(
            assigned_sender_from(Some("acme.com"), Some("rep@gmail.com"), Some("Alex Rep")),
            "\"Alex Rep\" <sales@granite-manager.com>"
        );
    }
}
