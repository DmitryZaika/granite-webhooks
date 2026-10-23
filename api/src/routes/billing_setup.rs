//! Superuser report on whether Stripe billing is set up correctly.
//!
//! - `GET /v1/superuser/billing/setup` → env variables, webhook endpoint and plan prices

use crate::auth::CurrentUser;
use crate::billing::setup::{
    EnvFacts, SetupCheck, env_checks, key_mode, price_check, webhook_checks,
};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

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
/// version) and for each configured plan price (active, monthly, USD, same
/// mode as the key). A Stripe call that fails becomes a failed check, not an
/// error. Stripe checks are skipped without `STRIPE_SECRET_KEY`. The report
/// is cached for 5 minutes. Super admins only.
#[utoipa::path(
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

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
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
    if state.stripe.is_enabled() {
        let mode = key_mode(&secret_key, &["sk_", "rk_"]);
        match state.stripe.webhook_endpoints().await {
            Ok(endpoints) => checks.extend(webhook_checks(Ok(&endpoints))),
            Err(err) => checks.extend(webhook_checks(Err(&err.to_string()))),
        }
        for plan in state.billing_plans.iter() {
            let price = state.stripe.price(&plan.price_id).await;
            checks.push(match &price {
                Ok(price) => price_check(plan, Ok(price), mode),
                Err(err) => price_check(plan, Err(&err.to_string()), mode),
            });
        }
    }
    BillingSetup {
        checks,
        checked_at: Utc::now(),
    }
}
