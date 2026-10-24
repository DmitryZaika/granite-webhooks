//! Payment setup: the signup's Stripe customer and SetupIntent.
//!
//! Every Stripe POST carries an idempotency key built from the signup id
//! (`signup:<id>:customer`, `signup:<id>:si:<attempt>`), so a retried request
//! never makes a second customer.

use super::intent::Intent;
use crate::billing::stripe::{SignupCustomer, StripeError};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use lambda_http::tracing;
use serde_json::Value;

/// Payment setups allowed per signup.
pub const MAX_SETUPS: u8 = 5;

/// What the page does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setup {
    /// The api attached a test payment method itself.
    Test,
    /// The page confirms the SetupIntent with Stripe's Payment Element.
    Stripe {
        client_secret: String,
        publishable_key: String,
    },
}

/// Test seam on: a test payment method is configured and the key is `sk_test_`.
pub fn is_test_mode(state: &AppState) -> bool {
    state.signup.test_payment_method.is_some() && state.stripe.is_test_mode()
}

fn invalid_state() -> ApiError {
    ApiError::coded(
        axum::http::StatusCode::CONFLICT,
        "invalid_state",
        "Verify your email first.",
    )
}

fn stripe_failed(err: &StripeError, signup_id: &str) -> ApiError {
    tracing::warn!(
        stripe_code = err.code(),
        signup = %super::funnel::distinct_id(signup_id),
        "signup payment setup failed at Stripe"
    );
    ApiError::Upstream("We could not set up payments. Try again in a moment.".into())
}

fn id_of(object: &Value) -> Option<&str> {
    object["id"].as_str().filter(|id| !id.is_empty())
}

/// Counts one setup attempt (at most [`MAX_SETUPS`]) and returns its number.
async fn count_attempt(state: &AppState, intent: &Intent) -> ApiResult<u8> {
    let done = sqlx::query(
        "UPDATE signup_intents
            SET setup_attempts = LAST_INSERT_ID(setup_attempts + 1), updated_at = ?
          WHERE id = ? AND status IN ('verified', 'payment_ready') AND setup_attempts < ?",
    )
    .bind(crate::billing::now())
    .bind(&intent.id)
    .bind(MAX_SETUPS)
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 1 {
        return Ok(u8::try_from(done.last_insert_id()).unwrap_or(MAX_SETUPS));
    }
    match super::intent::load(&state.pool, &intent.id).await? {
        Some(current) if current.setup_attempts >= MAX_SETUPS => Err(ApiError::coded(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "setup_limited",
            "Too many payment setups. Start again or email support@granite-manager.com.",
        )),
        _ => Err(invalid_state()),
    }
}

/// The signup's Stripe customer, created once (`stripe_customer_id` is
/// stored guarded; the idempotency key makes a racing creation return the
/// same customer).
async fn customer_id(state: &AppState, intent: &Intent) -> ApiResult<String> {
    if let Some(id) = &intent.stripe_customer_id {
        return Ok(id.clone());
    }
    let company = &intent.company;
    let customer = state
        .stripe
        .create_signup_customer(&SignupCustomer {
            signup_id: &intent.id,
            company_name: &company.name,
            email: &intent.email,
            phone: &company.phone,
            line1: &company.street,
            city: &company.city,
            state: &company.state,
            postal_code: &company.zip,
        })
        .await
        .map_err(|err| stripe_failed(&err, &intent.id))?;
    let id = id_of(&customer)
        .ok_or_else(|| ApiError::Upstream("We could not set up payments. Try again.".into()))?
        .to_string();
    sqlx::query(
        "UPDATE signup_intents SET stripe_customer_id = ?, updated_at = ?
          WHERE id = ? AND stripe_customer_id IS NULL",
    )
    .bind(&id)
    .bind(crate::billing::now())
    .bind(&intent.id)
    .execute(&state.pool)
    .await?;
    Ok(id)
}

/// Creates (or reuses) the customer and a new SetupIntent, stores it and moves
/// the signup to `payment_ready`. `intent` is `verified` or `payment_ready`.
pub async fn setup(state: &AppState, intent: &Intent) -> ApiResult<Setup> {
    let test = is_test_mode(state);
    let publishable_key = state.signup.publishable_key.clone();
    if !test && publishable_key.is_none() {
        return Err(super::unavailable());
    }
    let attempt = count_attempt(state, intent).await?;
    let customer = customer_id(state, intent).await?;
    let created = if let (true, Some(method)) = (test, &state.signup.test_payment_method) {
        state
            .stripe
            .create_confirmed_test_setup_intent(&customer, &intent.id, attempt, method)
            .await
    } else {
        state
            .stripe
            .create_signup_setup_intent(&customer, &intent.id, attempt)
            .await
    }
    .map_err(|err| stripe_failed(&err, &intent.id))?;
    let setup_intent = id_of(&created)
        .ok_or_else(|| ApiError::Upstream("We could not set up payments. Try again.".into()))?;
    let client_secret = created["client_secret"].as_str().map(str::to_string);
    if test && created["status"].as_str() != Some("succeeded") {
        tracing::warn!(
            signup = %super::funnel::distinct_id(&intent.id),
            "test payment method did not confirm"
        );
        return Err(ApiError::Upstream(
            "The test payment method was not accepted.".into(),
        ));
    }
    let stored = sqlx::query(
        "UPDATE signup_intents SET setup_intent_id = ?, status = 'payment_ready', updated_at = ?
          WHERE id = ? AND status IN ('verified', 'payment_ready')",
    )
    .bind(setup_intent)
    .bind(crate::billing::now())
    .bind(&intent.id)
    .execute(&state.pool)
    .await?;
    if stored.rows_affected() != 1 {
        return Err(invalid_state());
    }
    if test {
        return Ok(Setup::Test);
    }
    match (client_secret, publishable_key) {
        (Some(client_secret), Some(key)) => Ok(Setup::Stripe {
            client_secret,
            publishable_key: key.to_string(),
        }),
        _ => Err(ApiError::Upstream(
            "We could not set up payments. Try again.".into(),
        )),
    }
}
