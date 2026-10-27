//! Self-serve signup: a company buys Granite Manager on
//! payments.granite-manager.com without a superuser.
//!
//! The public flow (`routes::signup`, `/v1/public/signups/...`) keeps one
//! `signup_intents` row per signup: email code, plan, Stripe customer and
//! SetupIntent. Completion checks the consents the server rendered,
//! provisions the company, its owner and a 3-month-trial subscription under
//! idempotency keys built from the signup id, and signs the owner in. A
//! completion retried after a crash, or the `subscription-maintenance` job,
//! resumes where it stopped: one signup never makes two companies or two
//! subscriptions.
//!
//! The signup id is a bearer secret: never log it next to the email.

pub mod consent;
pub mod emails;
pub mod funnel;
pub mod intent;
pub mod maintenance;
pub mod notify;
pub mod payment;
pub mod provision;

use axum::http::HeaderMap;
use std::sync::Arc;

/// Signup settings from the environment.
#[derive(Clone, Default)]
pub struct SignupConfig {
    /// `SELF_SERVE_SIGNUP_ENABLED=1`: off, every `/v1/public/signups*` route
    /// answers 503 (`GET /v1/public/plans` still works).
    pub enabled: bool,
    /// `SIGNUP_TEST_PAYMENT_METHOD` (e.g. `pm_card_visa`): with a `sk_test_`
    /// key, payment setup confirms a SetupIntent with it server-side.
    pub test_payment_method: Option<Arc<str>>,
    /// `STRIPE_PUBLIC_KEY` (the publishable key), for the Payment Element.
    pub publishable_key: Option<Arc<str>>,
    /// `SIGNUP_NOTIFY_USER_ID`: the Granite Manager user (Dima) told about
    /// each new company on Telegram.
    pub notify_user_id: Option<i32>,
    /// `SIGNUP_NOTIFY_EMAIL`: where each new company is also emailed.
    pub notify_email: Option<Arc<str>>,
    /// `STRIPE_AUTOMATIC_TAX=1`: subscriptions compute sales tax.
    pub automatic_tax: bool,
}

impl SignupConfig {
    pub fn from_env() -> Self {
        let text = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let on = |name: &str| text(name).is_some_and(|value| value == "1" || value == "true");
        Self {
            enabled: on("SELF_SERVE_SIGNUP_ENABLED"),
            test_payment_method: text("SIGNUP_TEST_PAYMENT_METHOD").map(Arc::from),
            publishable_key: text("STRIPE_PUBLIC_KEY").map(Arc::from),
            notify_user_id: text("SIGNUP_NOTIFY_USER_ID").and_then(|id| id.parse().ok()),
            notify_email: text("SIGNUP_NOTIFY_EMAIL").map(Arc::from),
            automatic_tax: on("STRIPE_AUTOMATIC_TAX"),
        }
    }
}

/// Where a public request came from, kept with the consents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestMeta {
    pub ip: Option<String>,
    /// Capped at 512 characters.
    pub user_agent: Option<String>,
    /// The first `Accept-Language` tag, capped at 16 characters.
    pub locale: Option<String>,
}

impl RequestMeta {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let ip = crate::auth::client_ip(headers);
        Self {
            ip: (ip != "unknown").then(|| ip.chars().take(45).collect()),
            user_agent: header("user-agent").map(|agent| agent.chars().take(512).collect()),
            locale: header("accept-language")
                .and_then(|value| value.split([',', ';']).next())
                .map(|tag| tag.trim().chars().take(16).collect())
                .filter(|tag: &String| !tag.is_empty()),
        }
    }
}

/// The named lock serializing signups of one email:
/// `signup_email:<first 40 hex of SHA-256(email)>` (the email itself stays
/// out of MySQL's lock list, and the name fits its 64 characters).
pub fn email_lock_name(email: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(email.trim().to_lowercase().as_bytes());
    digest
        .iter()
        .take(20)
        .fold(String::from("signup_email:"), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// `POST /v1/public/signups*` answer this while `SELF_SERVE_SIGNUP_ENABLED` is off.
pub fn ensure_enabled(config: &SignupConfig) -> crate::error::ApiResult<()> {
    if config.enabled {
        Ok(())
    } else {
        Err(unavailable())
    }
}

/// 503 `signup_unavailable`.
pub fn unavailable() -> crate::error::ApiError {
    crate::error::ApiError::coded(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "signup_unavailable",
        "Signup is not available yet.",
    )
}

/// The price list signups quote and charge.
///
/// Refused (503 `signup_unavailable`) when it is unavailable or no longer
/// says what the consent texts state (the extra-user price and the included
/// users are written into them).
pub async fn signup_catalog(
    stripe: &crate::billing::stripe::Stripe,
) -> crate::error::ApiResult<crate::billing::catalog::Catalog> {
    let catalog = crate::billing::catalog::Catalog::load(stripe)
        .await
        .map_err(|err| {
            lambda_http::tracing::error!(error = %err.reason(), "signup catalog unavailable");
            unavailable()
        })?;
    if let Some(mismatch) = consent::catalog_mismatch(&catalog) {
        lambda_http::tracing::error!(%mismatch, "signup prices differ from the consent texts");
        return Err(unavailable());
    }
    Ok(catalog)
}
