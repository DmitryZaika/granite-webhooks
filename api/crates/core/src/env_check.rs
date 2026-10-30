//! Startup checks of the environment: a feature that is switched on must have
//! everything it needs, and optional values must be well formed.
//!
//! [`config_problems`] is pure over an [`EnvSnapshot`]; it names variables,
//! never their values. `AppState::from_env` refuses to start when it finds a
//! problem, and the superuser billing setup check can list the same problems.
//! [`config_warnings`] are logged and listed but never stop the api.

use crate::billing::setup::key_mode;
use crate::onboarding_call::calendly::scheduling_url;
use std::collections::HashMap;
use std::fmt;

/// Every variable [`config_problems`] looks at.
const WATCHED: [&str; 23] = [
    "SESSION_SECRET",
    "SELF_SERVE_SIGNUP_ENABLED",
    "STRIPE_SECRET_KEY",
    "STRIPE_PUBLIC_KEY",
    "STRIPE_WEBHOOK_SECRET",
    "STRIPE_API_BASE",
    "STRIPE_AUTOMATIC_TAX",
    "WEBHOOKS_URL",
    "WEBHOOKS_KEY",
    "SIGNUP_NOTIFY_USER_ID",
    "SIGNUP_NOTIFY_EMAIL",
    "BILLING_ALERT_EMAIL",
    "BILLING_EMAIL_FROM_ADDRESS",
    "ONBOARDING_ADVANCE_CHARGING",
    "CALENDLY_API_TOKEN",
    "CALENDLY_BASE_URL",
    "CALENDLY_ONBOARDING_URL",
    "APP_URL",
    "PAYMENTS_APP_URL",
    "SESSION_COOKIE_DOMAIN",
    "LAMBDA_KEY",
    "API_ALLOWED_ORIGINS",
    "CLOUDFRONT_ORIGIN_SECRET",
];

/// The watched variables as the process has them (blank ones left out).
#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot(HashMap<String, String>);

impl EnvSnapshot {
    pub fn from_process() -> Self {
        Self(
            WATCHED
                .iter()
                .filter_map(|name| {
                    let value = std::env::var(name).ok()?;
                    let value = value.trim();
                    (!value.is_empty()).then(|| ((*name).to_string(), value.to_string()))
                })
                .collect(),
        )
    }

    /// For tests: only the pairs given.
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .filter(|(_, value)| !value.trim().is_empty())
                .map(|(name, value)| ((*name).to_string(), value.trim().to_string()))
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// `1`/`true` is on, `0`/`false` is off; anything else is `None`.
pub fn parse_flag(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// The variable `name` as a switch: set and on. A malformed value is off
/// here and reported by [`config_problems`].
pub fn flag_on(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .and_then(|value| parse_flag(&value))
        .unwrap_or(false)
}

fn looks_like_email(value: &str) -> bool {
    value.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && domain.contains('.')
            && !value.contains(char::is_whitespace)
            && !domain.ends_with('.')
    })
}

fn looks_like_url(value: &str) -> bool {
    value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(char::is_whitespace))
}

/// Stripe's keys must look like theirs and be from one mode.
fn stripe_key_problems(env: &EnvSnapshot, problems: &mut Vec<String>) {
    let secret_mode = env
        .get("STRIPE_SECRET_KEY")
        .map(|key| key_mode(key, &["sk_", "rk_"]));
    if secret_mode == Some(None) {
        problems.push("STRIPE_SECRET_KEY is not a Stripe secret key".to_string());
    }
    let public_mode = env
        .get("STRIPE_PUBLIC_KEY")
        .map(|key| key_mode(key, &["pk_"]));
    if public_mode == Some(None) {
        problems.push("STRIPE_PUBLIC_KEY is not a Stripe publishable key".to_string());
    }
    if let (Some(Some(secret)), Some(Some(public))) = (secret_mode, public_mode)
        && secret != public
    {
        problems.push(
            "STRIPE_PUBLIC_KEY and STRIPE_SECRET_KEY are from different Stripe modes".to_string(),
        );
    }
    if env
        .get("STRIPE_WEBHOOK_SECRET")
        .is_some_and(|secret| !secret.starts_with("whsec_"))
    {
        problems.push("STRIPE_WEBHOOK_SECRET is not a Stripe signing secret".to_string());
    }
}

/// What is missing or malformed, one sentence each, naming variables only.
/// Empty when the environment is sound. Everything switched off needs no
/// variable.
pub fn config_problems(env: &EnvSnapshot) -> Vec<String> {
    let mut problems = Vec::new();
    if env.get("SESSION_SECRET").is_none() {
        problems.push("SESSION_SECRET is not set".to_string());
    }

    let mut switch = |name: &str| -> bool {
        match env.get(name).map(parse_flag) {
            None => false,
            Some(Some(on)) => on,
            Some(None) => {
                problems.push(format!("{name} must be 1, true, 0 or false"));
                false
            }
        }
    };
    let signup = switch("SELF_SERVE_SIGNUP_ENABLED");
    let advance = switch("ONBOARDING_ADVANCE_CHARGING");
    switch("STRIPE_AUTOMATIC_TAX");

    let mut require = |feature: &str, names: &[&str]| {
        for name in names {
            if env.get(name).is_none() {
                problems.push(format!("{feature} is on but {name} is not set"));
            }
        }
    };
    if signup {
        require(
            "SELF_SERVE_SIGNUP_ENABLED",
            &[
                "STRIPE_SECRET_KEY",
                "STRIPE_PUBLIC_KEY",
                "STRIPE_WEBHOOK_SECRET",
                "WEBHOOKS_URL",
                "WEBHOOKS_KEY",
            ],
        );
    }
    if advance {
        require(
            "ONBOARDING_ADVANCE_CHARGING",
            &["STRIPE_SECRET_KEY", "CALENDLY_API_TOKEN"],
        );
    }

    if env.get("WEBHOOKS_URL").is_some() != env.get("WEBHOOKS_KEY").is_some() {
        problems.push("WEBHOOKS_URL and WEBHOOKS_KEY must be set together".to_string());
    }

    stripe_key_problems(env, &mut problems);

    if let Some(id) = env.get("SIGNUP_NOTIFY_USER_ID")
        && !id.parse::<i32>().is_ok_and(|id| id > 0)
    {
        problems.push("SIGNUP_NOTIFY_USER_ID is not a user id".to_string());
    }
    for name in [
        "SIGNUP_NOTIFY_EMAIL",
        "BILLING_ALERT_EMAIL",
        "BILLING_EMAIL_FROM_ADDRESS",
    ] {
        if env.get(name).is_some_and(|value| !looks_like_email(value)) {
            problems.push(format!("{name} is not an email address"));
        }
    }
    for name in [
        "STRIPE_API_BASE",
        "CALENDLY_BASE_URL",
        "WEBHOOKS_URL",
        "APP_URL",
        "PAYMENTS_APP_URL",
    ] {
        if env.get(name).is_some_and(|value| !looks_like_url(value)) {
            problems.push(format!("{name} is not an http(s) URL"));
        }
    }
    if env
        .get("CALENDLY_ONBOARDING_URL")
        .is_some_and(|url| scheduling_url(url).is_none())
    {
        problems.push("CALENDLY_ONBOARDING_URL is not a Calendly page".to_string());
    }
    problems
}

/// The warning when `CLOUDFRONT_ORIGIN_SECRET` is unset.
pub const ORIGIN_SECRET_WARNING: &str = "CLOUDFRONT_ORIGIN_SECRET is not set: X-Forwarded-For \
     is trusted on direct calls to the function URL, so the per-IP signup limit and login \
     throttle can be bypassed";

/// What is weaker than it should be but does not stop the api, one sentence
/// each, naming variables only.
pub fn config_warnings(env: &EnvSnapshot) -> Vec<String> {
    let mut warnings = Vec::new();
    if env.get("CLOUDFRONT_ORIGIN_SECRET").is_none() {
        warnings.push(ORIGIN_SECRET_WARNING.to_string());
    }
    warnings
}

/// Why the api does not start: the problems of [`config_problems`].
#[derive(Debug)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: {}", self.0.join("; "))
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: [(&str, &str); 1] = [("SESSION_SECRET", "s")];

    fn problems(extra: &[(&str, &str)]) -> Vec<String> {
        let pairs: Vec<(&str, &str)> = BASE.iter().chain(extra).copied().collect();
        config_problems(&EnvSnapshot::from_pairs(&pairs))
    }

    const SIGNUP_READY: [(&str, &str); 6] = [
        ("SELF_SERVE_SIGNUP_ENABLED", "1"),
        ("STRIPE_SECRET_KEY", "sk_test_a"),
        ("STRIPE_PUBLIC_KEY", "pk_test_a"),
        ("STRIPE_WEBHOOK_SECRET", "whsec_a"),
        ("WEBHOOKS_URL", "https://hooks.example.com"),
        ("WEBHOOKS_KEY", "k"),
    ];

    #[test]
    fn everything_off_needs_only_the_session_secret() {
        assert!(problems(&[]).is_empty());
        let none = config_problems(&EnvSnapshot::default());
        assert_eq!(none, ["SESSION_SECRET is not set"]);
    }

    #[test]
    fn signup_on_names_each_missing_dependency() {
        let found = problems(&[("SELF_SERVE_SIGNUP_ENABLED", "1")]);
        for name in [
            "STRIPE_SECRET_KEY",
            "STRIPE_PUBLIC_KEY",
            "STRIPE_WEBHOOK_SECRET",
            "WEBHOOKS_URL",
            "WEBHOOKS_KEY",
        ] {
            assert!(
                found.iter().any(|problem| problem.contains(name)),
                "{name} missing from {found:?}"
            );
        }
        assert!(problems(&SIGNUP_READY).is_empty());
    }

    #[test]
    fn signup_off_ignores_missing_dependencies() {
        assert!(problems(&[("SELF_SERVE_SIGNUP_ENABLED", "0")]).is_empty());
    }

    #[test]
    fn a_malformed_switch_is_a_problem() {
        let found = problems(&[("SELF_SERVE_SIGNUP_ENABLED", "yes")]);
        assert_eq!(
            found,
            ["SELF_SERVE_SIGNUP_ENABLED must be 1, true, 0 or false"]
        );
        assert_eq!(problems(&[("STRIPE_AUTOMATIC_TAX", "on")]).len(), 1);
    }

    #[test]
    fn a_malformed_notify_user_id_is_a_problem() {
        for bad in ["dima", "0", "-3", "1.5"] {
            let found = problems(&[("SIGNUP_NOTIFY_USER_ID", bad)]);
            assert_eq!(found, ["SIGNUP_NOTIFY_USER_ID is not a user id"], "{bad}");
        }
        assert!(problems(&[("SIGNUP_NOTIFY_USER_ID", "42")]).is_empty());
    }

    #[test]
    fn advance_charging_needs_calendly_and_stripe() {
        let found = problems(&[("ONBOARDING_ADVANCE_CHARGING", "1")]);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|p| p.contains("CALENDLY_API_TOKEN")));
        assert!(found.iter().any(|p| p.contains("STRIPE_SECRET_KEY")));
        assert!(
            problems(&[
                ("ONBOARDING_ADVANCE_CHARGING", "1"),
                ("CALENDLY_API_TOKEN", "t"),
                ("STRIPE_SECRET_KEY", "sk_live_a"),
            ])
            .is_empty()
        );
    }

    #[test]
    fn half_a_webhooks_target_is_a_problem() {
        let found = problems(&[("WEBHOOKS_URL", "https://hooks.example.com")]);
        assert_eq!(
            found,
            ["WEBHOOKS_URL and WEBHOOKS_KEY must be set together"]
        );
    }

    #[test]
    fn stripe_keys_must_look_right_and_agree() {
        assert_eq!(
            problems(&[("STRIPE_SECRET_KEY", "whatever")]),
            ["STRIPE_SECRET_KEY is not a Stripe secret key"]
        );
        assert_eq!(
            problems(&[("STRIPE_PUBLIC_KEY", "sk_test_a")]),
            ["STRIPE_PUBLIC_KEY is not a Stripe publishable key"]
        );
        assert_eq!(
            problems(&[
                ("STRIPE_SECRET_KEY", "sk_live_a"),
                ("STRIPE_PUBLIC_KEY", "pk_test_a"),
            ]),
            ["STRIPE_PUBLIC_KEY and STRIPE_SECRET_KEY are from different Stripe modes"]
        );
        assert_eq!(
            problems(&[("STRIPE_WEBHOOK_SECRET", "abc")]),
            ["STRIPE_WEBHOOK_SECRET is not a Stripe signing secret"]
        );
    }

    #[test]
    fn optional_addresses_and_urls_must_be_well_formed() {
        for name in [
            "SIGNUP_NOTIFY_EMAIL",
            "BILLING_ALERT_EMAIL",
            "BILLING_EMAIL_FROM_ADDRESS",
        ] {
            assert_eq!(
                problems(&[(name, "not an email")]),
                [format!("{name} is not an email address")]
            );
        }
        assert_eq!(
            problems(&[("CALENDLY_BASE_URL", "calendly.test")]),
            ["CALENDLY_BASE_URL is not an http(s) URL"]
        );
        assert_eq!(
            problems(&[("CALENDLY_ONBOARDING_URL", "https://example.com/x")]),
            ["CALENDLY_ONBOARDING_URL is not a Calendly page"]
        );
        assert!(
            problems(&[
                (
                    "CALENDLY_ONBOARDING_URL",
                    "https://calendly.com/dima/onboarding"
                ),
                ("BILLING_EMAIL_FROM_ADDRESS", "billing@granite-manager.com"),
            ])
            .is_empty()
        );
    }

    #[test]
    fn a_missing_origin_secret_is_a_warning_not_a_problem() {
        let env = EnvSnapshot::from_pairs(&BASE);
        assert!(config_problems(&env).is_empty());
        assert_eq!(config_warnings(&env), [ORIGIN_SECRET_WARNING]);
        let env = EnvSnapshot::from_pairs(&[("CLOUDFRONT_ORIGIN_SECRET", "secret-value")]);
        assert!(config_warnings(&env).is_empty());
    }

    #[test]
    fn problems_never_contain_values() {
        let found = problems(&[
            ("STRIPE_SECRET_KEY", "super-secret-value"),
            ("SIGNUP_NOTIFY_EMAIL", "person-at-example"),
            ("SIGNUP_NOTIFY_USER_ID", "private-id"),
        ]);
        assert_eq!(found.len(), 3);
        let joined = found.join(" ");
        for value in ["super-secret-value", "person-at-example", "private-id"] {
            assert!(!joined.contains(value));
        }
    }
}
