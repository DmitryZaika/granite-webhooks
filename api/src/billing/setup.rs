//! The checks behind `GET /v1/superuser/billing/setup`.
//!
//! Pure builders over facts the route gathered from the environment and from
//! Stripe; they never return key values, only whether they are set and which
//! mode they are in.

use super::BillingPlan;
use super::stripe::API_VERSION;
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// Path of our Stripe webhook route.
pub const WEBHOOK_PATH: &str = "/v1/webhooks/stripe";

/// The Stripe events the webhook endpoint must send: the ones
/// `normalize_event` acts on, and the self-serve subscription signals
/// (02-architecture.md section 5).
pub const REQUIRED_EVENTS: [&str; 9] = [
    "checkout.session.completed",
    "customer.subscription.created",
    "customer.subscription.updated",
    "customer.subscription.deleted",
    "invoice.paid",
    "invoice.payment_failed",
    "payment_method.updated",
    "customer.subscription.trial_will_end",
    "invoice.finalization_failed",
];

/// One line of the billing setup report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SetupCheck {
    /// Stable machine name, e.g. `stripe_secret_key`, `webhook_endpoint`,
    /// `price_price_123` (one per configured plan price).
    pub key: String,
    /// Short human title of what is checked.
    pub label: String,
    /// True when this part of the setup is correct.
    pub ok: bool,
    /// What was found or what is wrong. Never contains a key or secret value.
    pub detail: String,
}

fn check(key: &str, label: &str, ok: bool, detail: impl Into<String>) -> SetupCheck {
    SetupCheck {
        key: key.into(),
        label: label.into(),
        ok,
        detail: detail.into(),
    }
}

/// Live or test, read from a Stripe key's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    Live,
    Test,
}

impl KeyMode {
    const fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Test => "test",
        }
    }
}

/// The mode of a key like `sk_live_...` / `rk_test_...`, `None` when it is not one.
pub fn key_mode(key: &str, prefixes: &[&str]) -> Option<KeyMode> {
    let key = key.trim();
    prefixes.iter().find_map(|prefix| {
        let rest = key.strip_prefix(prefix)?;
        if rest.starts_with("live_") {
            Some(KeyMode::Live)
        } else if rest.starts_with("test_") {
            Some(KeyMode::Test)
        } else {
            None
        }
    })
}

/// The environment values billing depends on, as read by the route.
pub struct EnvFacts<'a> {
    pub secret_key: &'a str,
    pub publishable_key: &'a str,
    pub webhook_secret: &'a str,
    pub plans: &'a [BillingPlan],
    pub app_url: &'a str,
    pub webhooks_url: &'a str,
    pub webhooks_key: &'a str,
    pub job_key_set: bool,
}

fn money(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Checks of the environment variables.
pub fn env_checks(env: &EnvFacts) -> Vec<SetupCheck> {
    let secret_mode = key_mode(env.secret_key, &["sk_", "rk_"]);
    let public_mode = key_mode(env.publishable_key, &["pk_"]);
    let mut checks = vec![
        check(
            "stripe_secret_key",
            "Stripe secret key",
            secret_mode.is_some(),
            secret_mode.map_or_else(
                || "STRIPE_SECRET_KEY is not set or is not an sk_/rk_ key".to_string(),
                |mode| format!("{} mode", mode.name()),
            ),
        ),
        check(
            "stripe_publishable_key",
            "Stripe publishable key",
            public_mode.is_some() && public_mode == secret_mode,
            match (public_mode, secret_mode) {
                (None, _) => {
                    "STRIPE_PUBLIC_KEY is not set: the card payment page will not work".into()
                }
                (Some(public), Some(secret)) if public != secret => format!(
                    "{} mode, but the secret key is {} mode",
                    public.name(),
                    secret.name()
                ),
                (Some(public), None) => {
                    format!("{} mode, but the secret key is missing", public.name())
                }
                (Some(public), Some(_)) => format!("{} mode", public.name()),
            },
        ),
    ];
    let signing = env.webhook_secret.trim().starts_with("whsec_");
    checks.push(check(
        "stripe_webhook_secret",
        "Webhook signing secret",
        signing,
        if signing {
            "set"
        } else {
            "STRIPE_WEBHOOK_SECRET is not set or does not start with whsec_: payments will not unlock access"
        },
    ));
    checks.push(check(
        "billing_plans",
        "Billing plans",
        !env.plans.is_empty(),
        if env.plans.is_empty() {
            "STRIPE_BILLING_PLANS is not set".to_string()
        } else {
            env.plans
                .iter()
                .map(|plan| {
                    plan.monthly_price_cents.map_or_else(
                        || format!("{}: price from Stripe", plan.name),
                        |cents| format!("{}: {} USD", plan.name, money(cents)),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        },
    ));
    let app_url = env.app_url.trim();
    checks.push(check(
        "app_url",
        "App URL",
        app_url.starts_with("https://"),
        if app_url.is_empty() {
            "APP_URL is not set: billing email links use https://granite-manager.com".to_string()
        } else if app_url.starts_with("https://") {
            app_url.to_string()
        } else {
            format!("{app_url} is not an https:// URL")
        },
    ));
    let email = !env.webhooks_url.trim().is_empty() && !env.webhooks_key.trim().is_empty();
    checks.push(check(
        "email_sending",
        "Email sending",
        email,
        if email {
            "WEBHOOKS_URL and WEBHOOKS_KEY are set"
        } else {
            "WEBHOOKS_URL or WEBHOOKS_KEY is not set: billing reminder emails cannot be sent"
        },
    ));
    checks.push(check(
        "lambda_key",
        "Scheduled jobs key",
        env.job_key_set,
        if env.job_key_set {
            "set"
        } else {
            "LAMBDA_KEY is not set: the scheduled billing reminders job cannot run"
        },
    ));
    checks
}

fn endpoint_path(url: &str) -> &str {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    url.trim_end_matches('/')
}

fn enabled_events(endpoint: &Value) -> Vec<&str> {
    endpoint["enabled_events"]
        .as_array()
        .map(|events| events.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Checks of the webhook endpoint as Stripe lists it (or why it could not be read).
pub fn webhook_checks(endpoints: Result<&[Value], &str>) -> Vec<SetupCheck> {
    const KEY: &str = "webhook_endpoint";
    const LABEL: &str = "Webhook endpoint in Stripe";
    let endpoints = match endpoints {
        Ok(endpoints) => endpoints,
        Err(error) => {
            return vec![check(
                KEY,
                LABEL,
                false,
                format!("Could not read webhook endpoints: {error}"),
            )];
        }
    };
    let enabled: Vec<&Value> = endpoints
        .iter()
        .filter(|endpoint| endpoint["status"] == "enabled")
        .collect();
    let (ours, others): (Vec<&Value>, Vec<&Value>) = enabled.iter().partition(|endpoint| {
        endpoint_path(endpoint["url"].as_str().unwrap_or_default()).ends_with(WEBHOOK_PATH)
    });
    let warning = if others.is_empty() {
        String::new()
    } else {
        format!(
            "; {} other enabled endpoint(s) also receive events, and two consumers would share event ids",
            others.len()
        )
    };
    let [endpoint] = ours.as_slice() else {
        let detail = if ours.is_empty() {
            let disabled = endpoints.iter().any(|endpoint| {
                endpoint_path(endpoint["url"].as_str().unwrap_or_default()).ends_with(WEBHOOK_PATH)
            });
            if disabled {
                format!(
                    "The endpoint for *{WEBHOOK_PATH} is disabled: payments will not unlock access{warning}"
                )
            } else {
                format!("No endpoint for *{WEBHOOK_PATH}: payments will not unlock access{warning}")
            }
        } else {
            format!(
                "{} enabled endpoints for *{WEBHOOK_PATH}: each event would be handled twice{warning}",
                ours.len()
            )
        };
        return vec![check(KEY, LABEL, false, detail)];
    };
    let url = endpoint["url"].as_str().unwrap_or_default();
    let events = enabled_events(endpoint);
    let missing: Vec<&str> = if events.contains(&"*") {
        Vec::new()
    } else {
        REQUIRED_EVENTS
            .into_iter()
            .filter(|event| !events.contains(event))
            .collect()
    };
    let version = endpoint["api_version"].as_str();
    vec![
        check(
            KEY,
            LABEL,
            others.is_empty(),
            format!("{url} is enabled{warning}"),
        ),
        check(
            "webhook_events",
            "Webhook events",
            missing.is_empty(),
            if missing.is_empty() {
                format!("all {} events enabled", REQUIRED_EVENTS.len())
            } else {
                format!("Missing events: {}", missing.join(", "))
            },
        ),
        check(
            "webhook_api_version",
            "Webhook API version",
            version == Some(API_VERSION),
            if version == Some(API_VERSION) {
                API_VERSION.to_string()
            } else {
                format!(
                    "{}, the api expects {API_VERSION}",
                    version.unwrap_or("account default")
                )
            },
        ),
    ]
}

/// The check for one configured plan price as Stripe reports it. `key_mode` is
/// the secret key's mode when known.
pub fn price_check(
    plan: &BillingPlan,
    price: Result<&Value, &str>,
    key_mode: Option<KeyMode>,
) -> SetupCheck {
    let key = format!("price_{}", plan.price_id);
    let label = format!("Price for {}", plan.name);
    let price = match price {
        Ok(price) => price,
        Err(error) => {
            return check(
                &key,
                &label,
                false,
                format!("Could not read {}: {error}", plan.price_id),
            );
        }
    };
    let mut problems = price_problems(price, key_mode);
    let amount = price["unit_amount"].as_i64();
    if let (Some(configured), Some(amount)) = (plan.monthly_price_cents, amount)
        && configured != amount
    {
        problems.push(format!(
            "Stripe charges {} USD but STRIPE_BILLING_PLANS says {} USD",
            money(amount),
            money(configured)
        ));
    }
    let detail = if problems.is_empty() {
        amount.map_or_else(
            || "monthly price".to_string(),
            |cents| format!("{} USD per month", money(cents)),
        )
    } else {
        problems.join(", ")
    };
    check(&key, &label, problems.is_empty(), detail)
}

/// What is wrong with a monthly price: inactive, not monthly, not USD, or
/// in the other mode than the key.
fn price_problems(price: &Value, key_mode: Option<KeyMode>) -> Vec<String> {
    let mut problems = Vec::new();
    if price["active"] != true {
        problems.push("inactive".to_string());
    }
    let recurring = &price["recurring"];
    if recurring["interval"] != "month" || recurring["interval_count"].as_i64().unwrap_or(1) != 1 {
        problems.push("not a monthly price".into());
    }
    let currency = price["currency"].as_str().unwrap_or("unknown");
    if currency != "usd" {
        problems.push(format!("currency {currency}"));
    }
    if let (Some(mode), Some(live)) = (key_mode, price["livemode"].as_bool())
        && live != (mode == KeyMode::Live)
    {
        problems.push(
            if live {
                "live price with a test key"
            } else {
                "test price with a live key"
            }
            .into(),
        );
    }
    problems
}

/// The checks of the self-serve prices by lookup key (`billing::catalog`).
///
/// One check per key (found, active, monthly, USD, per unit, same mode as the
/// key), then whether the catalog still says what the consent texts state.
/// `prices` is what `GET /v1/prices?lookup_keys[]=` answered, or why it failed.
pub fn catalog_checks(
    lookup_keys: &[&str],
    prices: Result<&[Value], &str>,
    key_mode: Option<KeyMode>,
    consent_mismatch: Option<&str>,
) -> Vec<SetupCheck> {
    let prices = match prices {
        Ok(prices) => prices,
        Err(error) => {
            return vec![check(
                "signup_prices",
                "Self-serve prices",
                false,
                format!("Could not read the self-serve prices: {error}"),
            )];
        }
    };
    let mut checks: Vec<SetupCheck> = lookup_keys
        .iter()
        .map(|lookup_key| {
            let key = format!("signup_price_{lookup_key}");
            let label = format!("Self-serve price {lookup_key}");
            let Some(price) = prices
                .iter()
                .find(|price| price["lookup_key"] == *lookup_key)
            else {
                return check(
                    &key,
                    &label,
                    false,
                    "No active price has this lookup key: self-serve signup is unavailable",
                );
            };
            let mut problems = price_problems(price, key_mode);
            if price["billing_scheme"] != "per_unit" {
                problems.push("not a per-unit price".into());
            }
            let amount = price["unit_amount"].as_i64();
            if amount.is_none() {
                problems.push("no unit amount".into());
            }
            let detail = if problems.is_empty() {
                amount.map_or_else(String::new, |cents| {
                    format!("{} USD per month", money(cents))
                })
            } else {
                problems.join(", ")
            };
            check(&key, &label, problems.is_empty(), detail)
        })
        .collect();
    checks.push(check(
        "signup_consents",
        "Self-serve prices match the consent texts",
        consent_mismatch.is_none(),
        consent_mismatch.map_or_else(
            || "the extra-user price and included users are what the checkboxes state".into(),
            |mismatch| format!("Signup is refused until a new consent version ships: {mismatch}"),
        ),
    ));
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan(cents: Option<i64>) -> BillingPlan {
        BillingPlan {
            price_id: "price_1".into(),
            name: "Pro".into(),
            monthly_price_cents: cents,
        }
    }

    fn good_env(plans: &[BillingPlan]) -> EnvFacts<'_> {
        EnvFacts {
            secret_key: "sk_live_x",
            publishable_key: "pk_live_x",
            webhook_secret: "whsec_x",
            plans,
            app_url: "https://granite-manager.com",
            webhooks_url: "https://hooks",
            webhooks_key: "k",
            job_key_set: true,
        }
    }

    fn by_key<'a>(checks: &'a [SetupCheck], key: &str) -> &'a SetupCheck {
        checks.iter().find(|c| c.key == key).unwrap()
    }

    fn endpoint(url: &str, status: &str, events: &[&str], version: &str) -> Value {
        json!({ "url": url, "status": status, "enabled_events": events, "api_version": version })
    }

    #[test]
    fn a_complete_environment_passes_without_leaking_values() {
        let plans = [plan(Some(4900))];
        let checks = env_checks(&good_env(&plans));
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        assert_eq!(by_key(&checks, "stripe_secret_key").detail, "live mode");
        let json = serde_json::to_string(&checks).unwrap();
        assert!(
            !json.contains("sk_live_x") && !json.contains("whsec_x") && !json.contains("pk_live_x")
        );
    }

    #[test]
    fn missing_or_mismatched_environment_fails_each_check() {
        let env = EnvFacts {
            secret_key: "sk_test_x",
            publishable_key: "pk_live_x",
            webhook_secret: "nope",
            plans: &[],
            app_url: "",
            webhooks_url: "https://hooks",
            webhooks_key: "",
            job_key_set: false,
        };
        let checks = env_checks(&env);
        assert!(by_key(&checks, "stripe_secret_key").ok);
        assert_eq!(by_key(&checks, "stripe_secret_key").detail, "test mode");
        for key in [
            "stripe_publishable_key",
            "stripe_webhook_secret",
            "billing_plans",
            "app_url",
            "email_sending",
            "lambda_key",
        ] {
            assert!(!by_key(&checks, key).ok, "{key}");
        }
        assert!(
            by_key(&checks, "stripe_publishable_key")
                .detail
                .contains("live mode, but")
        );
        assert!(
            !env_checks(&EnvFacts {
                secret_key: "",
                ..good_env(&[])
            })[0]
                .ok
        );
    }

    #[test]
    fn a_single_good_endpoint_passes_all_three_checks() {
        let all = REQUIRED_EVENTS;
        let list = [endpoint(
            "https://api.x.com/v1/webhooks/stripe",
            "enabled",
            &all,
            API_VERSION,
        )];
        let checks = webhook_checks(Ok(&list));
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        let star = [endpoint(
            "https://api.x.com/v1/webhooks/stripe",
            "enabled",
            &["*"],
            API_VERSION,
        )];
        assert!(webhook_checks(Ok(&star)).iter().all(|c| c.ok));
    }

    #[test]
    fn endpoint_problems_are_reported() {
        let events_without_paid: Vec<&str> = REQUIRED_EVENTS
            .into_iter()
            .filter(|e| *e != "invoice.paid")
            .collect();
        let list = [endpoint(
            "https://x/v1/webhooks/stripe",
            "enabled",
            &events_without_paid,
            "2020-01-01",
        )];
        let checks = webhook_checks(Ok(&list));
        assert_eq!(
            by_key(&checks, "webhook_events").detail,
            "Missing events: invoice.paid"
        );
        assert!(
            by_key(&checks, "webhook_api_version")
                .detail
                .contains("2020-01-01")
        );

        let none = webhook_checks(Ok(&[]));
        assert_eq!(none.len(), 1);
        assert!(!none[0].ok);
        let disabled = [endpoint(
            "https://x/v1/webhooks/stripe",
            "disabled",
            &["*"],
            API_VERSION,
        )];
        assert!(webhook_checks(Ok(&disabled))[0].detail.contains("disabled"));
        let error = webhook_checks(Err("boom"));
        assert!(!error[0].ok && error[0].detail.contains("boom"));
    }

    #[test]
    fn duplicate_or_extra_endpoints_are_flagged() {
        let ours = endpoint(
            "https://x/v1/webhooks/stripe",
            "enabled",
            &["*"],
            API_VERSION,
        );
        let twice = [
            ours.clone(),
            endpoint(
                "https://y/v1/webhooks/stripe",
                "enabled",
                &["*"],
                API_VERSION,
            ),
        ];
        let checks = webhook_checks(Ok(&twice));
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].ok);
        let extra = [
            ours,
            endpoint(
                "https://old/api/stripe/webhook",
                "enabled",
                &["*"],
                API_VERSION,
            ),
        ];
        let checks = webhook_checks(Ok(&extra));
        assert!(!checks[0].ok);
        assert!(checks[0].detail.contains("share event ids"));
    }

    #[test]
    fn prices_must_be_active_monthly_usd_and_match_the_key_mode() {
        let good = json!({ "active": true, "currency": "usd", "livemode": false,
            "unit_amount": 4900, "recurring": { "interval": "month", "interval_count": 1 } });
        let ok = price_check(&plan(Some(4900)), Ok(&good), Some(KeyMode::Test));
        assert!(ok.ok, "{ok:?}");
        assert_eq!(ok.detail, "49.00 USD per month");
        assert_eq!(ok.key, "price_price_1");

        let bad = json!({ "active": false, "currency": "eur", "livemode": true,
            "unit_amount": 5000, "recurring": { "interval": "year", "interval_count": 1 } });
        let result = price_check(&plan(Some(4900)), Ok(&bad), Some(KeyMode::Test));
        assert!(!result.ok);
        for part in [
            "inactive",
            "not a monthly price",
            "currency eur",
            "live price with a test key",
            "50.00 USD",
        ] {
            assert!(result.detail.contains(part), "{part}: {}", result.detail);
        }
        assert!(
            !price_check(&plan(None), Ok(&bad), None)
                .detail
                .contains("with a")
        );
        let missing = price_check(&plan(None), Err("No such price"), None);
        assert!(!missing.ok && missing.detail.contains("No such price"));
    }

    #[test]
    fn the_self_serve_events_are_required() {
        let without_trial: Vec<&str> = REQUIRED_EVENTS
            .into_iter()
            .filter(|e| *e != "customer.subscription.trial_will_end")
            .collect();
        let list = [endpoint(
            "https://x/v1/webhooks/stripe",
            "enabled",
            &without_trial,
            API_VERSION,
        )];
        let checks = webhook_checks(Ok(&list));
        assert_eq!(
            by_key(&checks, "webhook_events").detail,
            "Missing events: customer.subscription.trial_will_end"
        );
        assert!(REQUIRED_EVENTS.contains(&"invoice.finalization_failed"));
    }

    fn lookup_price(key: &str, cents: i64) -> Value {
        json!({ "id": format!("price_{key}"), "lookup_key": key, "active": true,
                "currency": "usd", "billing_scheme": "per_unit", "unit_amount": cents,
                "recurring": { "interval": "month", "interval_count": 1 }, "livemode": false })
    }

    #[test]
    fn self_serve_prices_are_checked_by_lookup_key() {
        let keys = ["granite_base_monthly_v1", "granite_extra_user_monthly_v1"];
        let mut tiered = lookup_price("granite_extra_user_monthly_v1", 3_000);
        tiered["billing_scheme"] = json!("tiered");
        let prices = [lookup_price("granite_base_monthly_v1", 30_000), tiered];
        let checks = catalog_checks(&keys, Ok(&prices), Some(KeyMode::Test), None);
        let base = by_key(&checks, "signup_price_granite_base_monthly_v1");
        assert!(base.ok);
        assert_eq!(base.detail, "300.00 USD per month");
        let extra = by_key(&checks, "signup_price_granite_extra_user_monthly_v1");
        assert!(!extra.ok);
        assert_eq!(extra.detail, "not a per-unit price");
        assert!(by_key(&checks, "signup_consents").ok);

        let missing = catalog_checks(&keys, Ok(&prices[..1]), None, None);
        assert!(!by_key(&missing, "signup_price_granite_extra_user_monthly_v1").ok);
        let live = catalog_checks(&keys[..1], Ok(&prices[..1]), Some(KeyMode::Live), None);
        assert!(!by_key(&live, "signup_price_granite_base_monthly_v1").ok);
    }

    #[test]
    fn prices_that_differ_from_the_consent_texts_fail_the_report() {
        let checks = catalog_checks(
            &[],
            Ok(&[]),
            None,
            Some("the extra-user price is $35.00 but the consent texts say $30.00"),
        );
        let consents = by_key(&checks, "signup_consents");
        assert!(!consents.ok);
        assert!(consents.detail.contains("$35.00"));
        let unreadable = catalog_checks(&["granite_base_monthly_v1"], Err("api_error"), None, None);
        assert_eq!(unreadable.len(), 1);
        assert!(!unreadable[0].ok);
    }
}
