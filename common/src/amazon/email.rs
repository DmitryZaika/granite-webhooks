//! Email through SES (v2 API), and the SES identities of the account.

use super::sigv4::{AWS_PATH, Signer};
use base64::Engine as _;
use percent_encoding::utf8_percent_encode;
use serde_json::{Value, json};

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

/// Sends an HTML email from `from`; returns SES's message id.
pub async fn send_message_from(
    to: &[&str],
    subject: &str,
    message: &str,
    from: &str,
) -> Result<String, Error> {
    let request = json!({
        "FromEmailAddress": from,
        "Destination": { "ToAddresses": to },
        "Content": { "Simple": {
            "Subject": { "Data": subject, "Charset": "UTF-8" },
            "Body": { "Html": { "Data": message, "Charset": "UTF-8" } },
        } },
    });
    let output = ses(
        "SendEmail",
        reqwest::Method::POST,
        "/v2/email/outbound-emails",
        Some(&request),
    )
    .await?;
    Ok(output["MessageId"].as_str().unwrap_or_default().to_string())
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
    let request = json!({
        "Destination": { "ToAddresses": destinations },
        "Content": { "Raw": { "Data": base64::engine::general_purpose::STANDARD.encode(raw) } },
    });
    let output = ses(
        "SendEmail",
        reqwest::Method::POST,
        "/v2/email/outbound-emails",
        Some(&request),
    )
    .await?;
    Ok(ses_message_id(
        output["MessageId"].as_str().unwrap_or_default(),
        SES_REGION,
    ))
}

/// A failed SES call: SES's answer, or why SES could not be asked.
#[derive(Debug)]
pub struct Error {
    operation: &'static str,
    /// The HTTP status SES answered with; `None` when it was not reached.
    status: Option<u16>,
    /// SES's error code (`MessageRejected`, `NotFoundException`, ...).
    code: String,
    message: String,
}

impl Error {
    fn unsent(operation: &'static str, message: String) -> Self {
        Self {
            operation,
            status: None,
            code: String::new(),
            message,
        }
    }

    fn is_not_found(&self) -> bool {
        self.status == Some(404) || self.code == "NotFoundException"
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SES {}", self.operation)?;
        if let Some(status) = self.status {
            write!(f, " {status}")?;
        }
        if !self.code.is_empty() {
            write!(f, " {}", self.code)?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for Error {}

/// One call of the SES v2 API in [`SES_REGION`] (`path_and_query` starts
/// with `/v2/email`), with `body` as its JSON; SES's JSON answer.
async fn ses(
    operation: &'static str,
    method: reqwest::Method,
    path_and_query: &str,
    body: Option<&Value>,
) -> Result<Value, Error> {
    let credentials = super::credentials::credentials()
        .await
        .map_err(|message| Error::unsent(operation, message))?;
    let url = format!("https://email.{SES_REGION}.amazonaws.com{path_and_query}");
    let body = body.map(Value::to_string).unwrap_or_default();
    let headers: &[(&str, &str)] = if body.is_empty() {
        &[]
    } else {
        &[("content-type", "application/json")]
    };
    let response = super::send(
        &Signer::new(credentials, SES_REGION, "ses"),
        method,
        &url,
        headers,
        body.into(),
        true,
    )
    .await
    .map_err(|message| Error::unsent(operation, message))?;
    let status = response.status();
    // `x-amzn-ErrorType: MessageRejected:http://internal.amazon.com/...`
    let code = response
        .headers()
        .get("x-amzn-errortype")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(':').next().unwrap_or_default().to_string());
    let text = response
        .text()
        .await
        .map_err(|error| Error::unsent(operation, error.without_url().to_string()))?;
    let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(answer);
    }
    let message = answer["message"]
        .as_str()
        .or_else(|| answer["Message"].as_str())
        .map_or_else(|| text.trim().to_string(), str::to_string);
    let code = code.unwrap_or_else(|| {
        // `{"__type": "com.amazonaws.sesv2#NotFoundException"}`
        let kind = answer["__type"].as_str().unwrap_or_default();
        kind.rsplit('#').next().unwrap_or_default().to_string()
    });
    Err(Error {
        operation,
        status: Some(status.as_u16()),
        code,
        message,
    })
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
    let path = format!(
        "/v2/email/identities/{}",
        utf8_percent_encode(&domain, AWS_PATH)
    );
    let lookup = ses("GetEmailIdentity", reqwest::Method::GET, &path, None);
    let verified = match tokio::time::timeout(std::time::Duration::from_secs(5), lookup).await {
        Ok(Ok(identity)) => identity["VerifiedForSendingStatus"].as_bool() == Some(true),
        Ok(Err(error)) => {
            if !error.is_not_found() {
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
        let mut verified = std::collections::HashSet::new();
        let mut pending = std::collections::HashSet::new();
        let mut next: Option<String> = None;
        loop {
            let mut path = "/v2/email/identities?PageSize=100".to_string();
            if let Some(token) = next.take() {
                path.push_str("&NextToken=");
                path.extend(utf8_percent_encode(&token, super::sigv4::AWS_QUERY));
            }
            let page = ses("ListEmailIdentities", reqwest::Method::GET, &path, None).await?;
            let identities = page["EmailIdentities"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            for identity in identities {
                let Some(name) = identity["IdentityName"]
                    .as_str()
                    .map(|n| n.trim().to_lowercase())
                else {
                    continue;
                };
                match identity["VerificationStatus"].as_str() {
                    Some("SUCCESS") => {
                        verified.insert(name);
                    }
                    Some("PENDING") => {
                        pending.insert(name);
                    }
                    _ => {}
                }
            }
            match page["NextToken"].as_str() {
                Some(token) if !token.is_empty() => next = Some(token.to_string()),
                _ => break,
            }
        }
        Ok::<_, Error>((verified, pending))
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
