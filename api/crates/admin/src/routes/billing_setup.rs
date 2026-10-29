//! Superuser report on whether Stripe billing is set up correctly.
//!
//! - `GET /v1/superuser/billing/setup` → env variables, webhook endpoint and plan prices

use crate::auth::CurrentUser;
use crate::billing::account::PORTAL_CONFIGURATION_TAG;
use crate::billing::catalog::{BASE_LOOKUP_KEY, Catalog, EXTRA_USER_LOOKUP_KEY};
use crate::billing::setup::{
    EnvFacts, ReleaseFacts, SetupCheck, catalog_checks, env_checks, key_mode,
    payment_method_checks, portal_checks, price_check, release_checks, startup_check,
    webhook_checks,
};
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_mins(5);

/// The last report by Stripe base URL, so the page does not hit Stripe on
/// every load.
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, BillingSetup)>>> =
    LazyLock::new(Mutex::default);

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(billing_setup))
}

/// Whether Stripe billing is set up correctly.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BillingSetup {
    /// One entry per thing checked, environment first, then what Stripe
    /// reports. Billing is fully set up when every entry has `ok = true`.
    pub checks: Vec<SetupCheck>,
    /// When the report was built (UTC, ISO 8601). Reports are reused for
    /// 5 minutes, so this can be a few minutes old.
    #[schema(value_type = String, format = DateTime)]
    pub checked_at: DateTime<Utc>,
}

/// Check the Stripe billing setup
///
/// Reports whether billing is set up correctly: the Stripe secret and
/// publishable keys (only whether they are set and live or test, never their
/// values), the webhook signing secret, the configured plans, `APP_URL`,
/// email sending and the scheduled jobs key, then what Stripe itself reports
/// for the webhook endpoint (exactly one enabled endpoint for
/// `/v1/webhooks/stripe`, with every handled event and the api's Stripe API
/// version), for the default payment method configuration (card and bank
/// debit enabled), for the customer portal configuration the api uses (it
/// exists, cancelling and plan changes off), the billing alert email,
/// payments app URL, signup notification target, Calendly token, onboarding
/// advance charging and the signup switch with what it depends on, and for each configured plan price (active, monthly, USD, same
/// mode as the key), each self-serve price by lookup key (same checks, per
/// unit) and whether those prices still match what the signup consent texts
/// state (signup refuses with 503 when not). A Stripe call that fails becomes
/// a failed check, not an error. Stripe checks are skipped without `STRIPE_SECRET_KEY`.
/// The report is cached for 5 minutes. Super admins only.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/billing/setup",
    operation_id = "get_billing_setup",
    tag = "superuser",
    responses(
        (status = 200, description = "The setup report", body = BillingSetup),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a super admin", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn billing_setup(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<BillingSetup>> {
    if !user.is_superuser {
        return Err(ApiError::Forbidden);
    }
    let base = state.stripe.base_url().to_string();
    let cached = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&base)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, report)| report.clone());
    if let Some(report) = cached {
        return Ok(Json(report));
    }
    let report = build(&state).await;
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(base, (Instant::now(), report.clone()));
    Ok(Json(report))
}

/// The self-serve price list by lookup key, and whether it still says what
/// the consent texts state (signup refuses with 503 otherwise).
async fn self_serve_checks(
    state: &AppState,
    mode: Option<crate::billing::setup::KeyMode>,
) -> Vec<SetupCheck> {
    let mut keys = vec![BASE_LOOKUP_KEY, EXTRA_USER_LOOKUP_KEY];
    keys.extend(AddonKey::ALL.map(AddonKey::lookup_key));
    let mismatch = match Catalog::load(&state.stripe).await {
        Ok(catalog) => crate::signup::consent::catalog_mismatch(&catalog),
        Err(err) => Some(format!(
            "the price list cannot be loaded ({})",
            err.reason()
        )),
    };
    match state.stripe.prices_by_lookup_keys(&keys).await {
        Ok(prices) => catalog_checks(&keys, Ok(&prices), mode, mismatch.as_deref()),
        Err(err) => catalog_checks(&keys, Err(err.code()), mode, mismatch.as_deref()),
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn env_on(name: &str) -> bool {
    matches!(env(name).trim(), "1" | "true")
}

async fn build(state: &AppState) -> BillingSetup {
    let secret_key = env("STRIPE_SECRET_KEY");
    let webhook_secret = state.stripe_webhook_secret.as_deref().unwrap_or_default();
    let mut checks = env_checks(&EnvFacts {
        secret_key: &secret_key,
        publishable_key: &env("STRIPE_PUBLIC_KEY"),
        webhook_secret,
        plans: &state.billing_plans,
        app_url: &env("APP_URL"),
        webhooks_url: &env("WEBHOOKS_URL"),
        webhooks_key: &env("WEBHOOKS_KEY"),
        job_key_set: state.job_key.is_some(),
    });
    checks.push(startup_check(&crate::env_check::config_problems(
        &crate::env_check::EnvSnapshot::from_process(),
    )));
    checks.extend(release_checks(&ReleaseFacts {
        billing_alert_email_set: state.billing_alert_email.is_some(),
        payments_app_url: &crate::billing::payments_app_url(),
        signup_enabled: state.signup.enabled,
        publishable_key_set: state.signup.publishable_key.is_some(),
        webhook_secret_set: !webhook_secret.trim().is_empty(),
        signup_notify_user_set: state.signup.notify_user_id.is_some(),
        signup_notify_email_set: state.signup.notify_email.is_some(),
        onboarding_advance_charging: env_on("ONBOARDING_ADVANCE_CHARGING"),
        calendly_token_set: state.onboarding_call.calendly.is_enabled(),
    }));
    if state.stripe.is_enabled() {
        let mode = key_mode(&secret_key, &["sk_", "rk_"]);
        match state.stripe.webhook_endpoints().await {
            Ok(endpoints) => checks.extend(webhook_checks(Ok(&endpoints))),
            Err(err) => checks.extend(webhook_checks(Err(&err.to_string()))),
        }
        match state.stripe.payment_method_configurations().await {
            Ok(configurations) => checks.extend(payment_method_checks(Ok(&configurations))),
            Err(err) => checks.extend(payment_method_checks(Err(&err.to_string()))),
        }
        match state.stripe.list_portal_configurations().await {
            Ok(configurations) => {
                checks.extend(portal_checks(Ok(&configurations), PORTAL_CONFIGURATION_TAG));
            }
            Err(err) => checks.extend(portal_checks(
                Err(&err.to_string()),
                PORTAL_CONFIGURATION_TAG,
            )),
        }
        for plan in state.billing_plans.iter() {
            let price = state.stripe.price(&plan.price_id).await;
            checks.push(match &price {
                Ok(price) => price_check(plan, Ok(price), mode),
                Err(err) => price_check(plan, Err(&err.to_string()), mode),
            });
        }
        checks.extend(self_serve_checks(state, mode).await);
    }
    BillingSetup {
        checks,
        checked_at: Utc::now(),
    }
}
