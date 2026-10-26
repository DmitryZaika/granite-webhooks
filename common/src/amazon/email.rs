use aws_sdk_sesv2::types::{Body, Content, Destination, EmailContent, Message};
use aws_sdk_sesv2::{Client, Error};

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

/// The address mail of a user leaves from: their own when its domain (the
/// part after the last `@`) is the company's mail domain or a subdomain of
/// it, else the shared sending address. A substring match would let
/// `acme.com@other-tenant.com` send as another company's verified domain.
pub fn from_email(company_domain: Option<&str>, user_email: &str) -> String {
    let domain = company_domain
        .map(|value| value.trim().trim_start_matches('@').to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let Some(domain) = domain else {
        return DEFAULT_SEND_EMAIL_ADDRESS.to_string();
    };
    let on_domain = user_email
        .trim()
        .rsplit_once('@')
        .is_some_and(|(local, host)| {
            let host = host.to_ascii_lowercase();
            !local.is_empty()
                && (host == domain
                    || host
                        .strip_suffix(domain.as_str())
                        .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')))
        });
    if on_domain {
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
    let shared_config = crate::http::aws_config("us-east-2").await;
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

/// The `Message-ID` SES gives a message it sent: `<id>@<region>.amazonses.com`
/// (`email.amazonses.com` in us-east-1), without angle brackets. Replies
/// carry it in `In-Reply-To`, which threads them.
pub fn ses_message_id(ses_id: &str, region: &str) -> String {
    if ses_id.contains('@') {
        return ses_id.to_string();
    }
    let host = if region == "us-east-1" {
        "email"
    } else {
        region
    };
    format!("{ses_id}@{host}.amazonses.com")
}

pub const SES_REGION: &str = "us-east-2";

/// Sends a complete MIME message to `destinations` (To, Cc and Bcc; Bcc is
/// not in the headers). Returns the message's `Message-ID` (see
/// [`ses_message_id`]).
pub async fn send_raw_message(raw: Vec<u8>, destinations: Vec<String>) -> Result<String, Error> {
    let shared_config = crate::http::aws_config(SES_REGION).await;
    let client = Client::new(&shared_config);
    let destination = Destination::builder()
        .set_to_addresses(Some(destinations))
        .build();
    let content = EmailContent::builder()
        .raw(
            aws_sdk_sesv2::types::RawMessage::builder()
                .data(aws_sdk_sesv2::primitives::Blob::new(raw))
                .build()
                .expect("raw message has data"),
        )
        .build();
    let output = client
        .send_email()
        .destination(destination)
        .content(content)
        .send()
        .await?;
    Ok(ses_message_id(
        output.message_id().unwrap_or(""),
        SES_REGION,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ses_message_id_adds_the_region_host() {
        assert_eq!(
            ses_message_id("010f-abc", "us-east-2"),
            "010f-abc@us-east-2.amazonses.com"
        );
        assert_eq!(
            ses_message_id("010f-abc", "us-east-1"),
            "010f-abc@email.amazonses.com"
        );
        assert_eq!(ses_message_id("x@y.com", "us-east-2"), "x@y.com");
    }

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
    fn from_email_matches_the_domain_not_a_substring() {
        for email in [
            "acme.com@other-tenant.com",
            "rep@acme.com.other-tenant.com",
            "rep@notacme.com",
            "rep@acme.co",
            "@acme.com",
            "rep.acme.com",
        ] {
            assert_eq!(
                from_email(Some("acme.com"), email),
                DEFAULT_SEND_EMAIL_ADDRESS,
                "{email}"
            );
        }
        assert_eq!(from_email(Some("Acme.com"), "Rep@ACME.com"), "Rep@ACME.com");
        assert_eq!(
            from_email(Some("acme.com"), "rep@mail.acme.com"),
            "rep@mail.acme.com"
        );
        assert_eq!(
            from_email(Some("@acme.com"), "rep@acme.com"),
            "rep@acme.com"
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
        assert_eq!(format_sender_from(None, "alex@acme.com"), "alex@acme.com");
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

/// How long a domain's SES verification is remembered.
const DOMAIN_CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(600);

type DomainChecks = std::collections::HashMap<String, (std::time::Instant, bool)>;

static DOMAIN_CHECKS: std::sync::LazyLock<std::sync::Mutex<DomainChecks>> =
    std::sync::LazyLock::new(Default::default);

/// Whether SES has verified `domain` for sending (`isSesDomainVerified`).
/// `None` when SES could not be asked (no permission, no network, timeout,
/// or `SES_DOMAIN_CHECK=off`); an unknown identity is `Some(false)`.
/// Answers are cached for ten minutes.
pub async fn domain_verified(domain: &str) -> Option<bool> {
    let domain = domain.trim().to_lowercase();
    if domain.is_empty() {
        return Some(false);
    }
    if std::env::var("SES_DOMAIN_CHECK").is_ok_and(|value| value.trim() == "off") {
        return None;
    }
    if let Ok(checks) = DOMAIN_CHECKS.lock()
        && let Some((at, verified)) = checks.get(&domain)
        && at.elapsed() < DOMAIN_CHECK_TTL
    {
        return Some(*verified);
    }
    let lookup = async {
        let shared_config = crate::http::aws_config("us-east-2").await;
        Client::new(&shared_config)
            .get_email_identity()
            .email_identity(&domain)
            .send()
            .await
    };
    let verified = match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
        Ok(Ok(identity)) => identity.verified_for_sending_status(),
        Ok(Err(error)) => {
            let not_found = error
                .as_service_error()
                .is_some_and(aws_sdk_sesv2::operation::get_email_identity::GetEmailIdentityError::is_not_found_exception);
            if !not_found {
                tracing::warn!(domain, %error, "SES GetEmailIdentity failed");
                return None;
            }
            false
        }
        Err(_) => {
            tracing::warn!(domain, "SES GetEmailIdentity timed out");
            return None;
        }
    };
    if let Ok(mut checks) = DOMAIN_CHECKS.lock() {
        checks.insert(domain, (std::time::Instant::now(), verified));
    }
    Some(verified)
}

/// SES identities of the account: (verified, pending), lowercased.
pub type IdentitySets = (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
);

static IDENTITY_SETS: std::sync::LazyLock<
    std::sync::Mutex<Option<(std::time::Instant, IdentitySets)>>,
> = std::sync::LazyLock::new(Default::default);

/// Every email identity of the SES account and whether it is verified
/// (`loadSesIdentitySets`). `None` when SES could not be asked (no
/// permission, timeout, or `SES_DOMAIN_CHECK=off`). Cached ten minutes.
pub async fn identity_sets() -> Option<IdentitySets> {
    if std::env::var("SES_DOMAIN_CHECK").is_ok_and(|value| value.trim() == "off") {
        return None;
    }
    if let Ok(cached) = IDENTITY_SETS.lock()
        && let Some((at, sets)) = cached.as_ref()
        && at.elapsed() < DOMAIN_CHECK_TTL
    {
        return Some(sets.clone());
    }
    let list = async {
        let shared_config = crate::http::aws_config("us-east-2").await;
        let client = Client::new(&shared_config);
        let mut verified = std::collections::HashSet::new();
        let mut pending = std::collections::HashSet::new();
        let mut next: Option<String> = None;
        loop {
            let page = client
                .list_email_identities()
                .page_size(100)
                .set_next_token(next.take())
                .send()
                .await?;
            for identity in page.email_identities() {
                let Some(name) = identity.identity_name().map(|n| n.trim().to_lowercase()) else {
                    continue;
                };
                match identity.verification_status() {
                    Some(aws_sdk_sesv2::types::VerificationStatus::Success) => {
                        verified.insert(name);
                    }
                    Some(aws_sdk_sesv2::types::VerificationStatus::Pending) => {
                        pending.insert(name);
                    }
                    _ => {}
                }
            }
            match page.next_token() {
                Some(token) => next = Some(token.to_string()),
                None => break,
            }
        }
        Ok::<
            _,
            aws_sdk_sesv2::error::SdkError<
                aws_sdk_sesv2::operation::list_email_identities::ListEmailIdentitiesError,
            >,
        >((verified, pending))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(5), list).await {
        Ok(Ok(sets)) => {
            if let Ok(mut cached) = IDENTITY_SETS.lock() {
                *cached = Some((std::time::Instant::now(), sets.clone()));
            }
            Some(sets)
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "SES ListEmailIdentities failed");
            None
        }
        Err(_) => {
            tracing::warn!("SES ListEmailIdentities timed out");
            None
        }
    }
}
