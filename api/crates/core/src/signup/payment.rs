//! Payment setup: the signup's Stripe customer and SetupIntent, and what a
//! SetupIntent says about its payment method.
//!
//! Every Stripe POST carries an idempotency key built from the signup id
//! (`signup:<id>:customer`, `signup:<id>:si:<attempt>`), so a retried request
//! never makes a second customer.

use super::consent::{PaymentFacts, PaymentMethodType};
use super::intent::{BANK_VERIFICATION_DAYS, Intent};
use crate::billing::stripe::{SetupPurpose, SignupCustomer, StripeError, stripe_id};
use crate::error::{ApiError, ApiResult};
use crate::openapi::ToSchema;
use crate::state::AppState;
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::Serialize;
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

/// Stripe down, throttled or unreachable: 503 `payments_unavailable`.
/// Stripe's refusals stay a 502 with a generic message (never Stripe's).
fn stripe_failed(err: &StripeError, signup_id: &str) -> ApiError {
    tracing::warn!(
        stripe_code = err.code(),
        signup = %super::funnel::distinct_id(signup_id),
        "signup payment setup failed at Stripe"
    );
    if err.is_retryable() || matches!(err, StripeError::NotConfigured) {
        return super::payments_unavailable();
    }
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
        return Err(super::payments_unavailable());
    }
    let attempt = count_attempt(state, intent).await?;
    let customer = customer_id(state, intent).await?;
    let created = if let (true, Some(method)) = (test, &state.signup.test_payment_method) {
        state
            .stripe
            .create_confirmed_test_setup_intent(
                &customer,
                SetupPurpose::Signup {
                    signup_id: &intent.id,
                    attempt,
                },
                method,
            )
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

/// Where a saved payment method is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SavedMethodStatus {
    /// Confirmed: it can be charged.
    Ready,
    /// A bank account waiting for its micro-deposits to be verified on
    /// Stripe's page (`verify_url`).
    Verifying,
}

/// A payment method saved on a SetupIntent, as the pages show it (read live
/// from Stripe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SavedMethod {
    pub status: SavedMethodStatus,
    /// `card` or `us_bank_account`.
    #[serde(rename = "type")]
    pub method_type: PaymentMethodType,
    /// The card brand (e.g. `visa`) or the bank's name; null when Stripe
    /// does not say.
    pub brand: Option<String>,
    /// The last 4 digits of the card or account number; null when unknown.
    pub last4: Option<String>,
    /// Stripe's page where the code of the test deposit is entered; only
    /// while `verifying`.
    pub verify_url: Option<String>,
    /// The bank account must be verified by then (ISO 8601, UTC); only while
    /// `verifying`.
    pub verify_by: Option<DateTime<Utc>>,
}

/// The SetupIntent waits for its bank account's micro-deposits to be
/// verified (`requires_action` with `verify_with_microdeposits`).
pub fn is_verifying(setup_intent: &Value) -> bool {
    setup_intent["status"] == "requires_action"
        && setup_intent["next_action"]["type"] == "verify_with_microdeposits"
}

/// The saved method of a SetupIntent, or `None` when nothing is usable.
///
/// Read with its `payment_method` expanded (`Stripe::setup_intent`):
/// `ready` when it succeeded, `verifying` while micro-deposits are pending.
/// A verifying one is due `created` + [`BANK_VERIFICATION_DAYS`]; callers
/// holding a stored deadline put theirs in `verify_by`.
pub fn saved_method(setup_intent: &Value) -> Option<SavedMethod> {
    let status = if setup_intent["status"] == "succeeded" {
        SavedMethodStatus::Ready
    } else if is_verifying(setup_intent) {
        SavedMethodStatus::Verifying
    } else {
        return None;
    };
    let method = &setup_intent["payment_method"];
    let method_type = PaymentMethodType::parse(method["type"].as_str()?)?;
    let details = &method[method_type.as_str()];
    let brand = match method_type {
        PaymentMethodType::Card => &details["brand"],
        PaymentMethodType::UsBankAccount => &details["bank_name"],
    };
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let (verify_url, verify_by) = if status == SavedMethodStatus::Verifying {
        let microdeposits = &setup_intent["next_action"]["verify_with_microdeposits"];
        let due = setup_intent["created"]
            .as_i64()
            .and_then(|created| DateTime::from_timestamp(created, 0))
            .map(|created| created + Duration::days(BANK_VERIFICATION_DAYS));
        (text(&microdeposits["hosted_verification_url"]), due)
    } else {
        (None, None)
    };
    Some(SavedMethod {
        status,
        method_type,
        brand: text(brand),
        last4: text(&details["last4"]),
        verify_url,
        verify_by,
    })
}

/// What the consent records keep about a SetupIntent's payment method.
///
/// Read with it expanded: id, type, last 4, bank name, mandate and the
/// intended SEC code (`company` account: CCD, `individual`: WEB). `None`
/// without a card or bank account.
pub fn payment_facts(customer: Option<String>, setup_intent: &Value) -> Option<PaymentFacts> {
    let method = &setup_intent["payment_method"];
    let payment_method_id = stripe_id(method)?;
    let method_type = PaymentMethodType::parse(method["type"].as_str()?)?;
    let details = &method[method_type.as_str()];
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let bank = method_type == PaymentMethodType::UsBankAccount;
    Some(PaymentFacts {
        stripe_customer_id: customer,
        payment_method_id: Some(payment_method_id),
        method_type: Some(method_type),
        last4: text(&details["last4"]),
        bank_name: if bank {
            text(&details["bank_name"])
        } else {
            None
        },
        mandate_id: if bank {
            stripe_id(&setup_intent["mandate"])
        } else {
            None
        },
        sec_code: match details["account_holder_type"].as_str() {
            Some("company") if bank => Some("CCD"),
            Some("individual") if bank => Some("WEB"),
            _ => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bank_setup(status: &str) -> Value {
        json!({
            "id": "seti_1", "status": status, "created": 1_800_000_000,
            "customer": "cus_1",
            "mandate": { "id": "mandate_1", "object": "mandate" },
            "next_action": {
                "type": "verify_with_microdeposits",
                "verify_with_microdeposits": {
                    "arrival_date": 1_800_100_000,
                    "hosted_verification_url": "https://payments.stripe.com/microdeposit/x",
                },
            },
            "payment_method": {
                "id": "pm_bank", "type": "us_bank_account",
                "us_bank_account": {
                    "bank_name": "STRIPE TEST BANK", "last4": "6789",
                    "account_holder_type": "company",
                },
            },
        })
    }

    #[test]
    fn a_bank_account_waiting_for_microdeposits_is_verifying() {
        let saved = saved_method(&bank_setup("requires_action")).unwrap();
        assert_eq!(saved.status, SavedMethodStatus::Verifying);
        assert_eq!(saved.method_type, PaymentMethodType::UsBankAccount);
        assert_eq!(saved.brand.as_deref(), Some("STRIPE TEST BANK"));
        assert_eq!(saved.last4.as_deref(), Some("6789"));
        assert_eq!(
            saved.verify_url.as_deref(),
            Some("https://payments.stripe.com/microdeposit/x")
        );
        assert_eq!(
            saved.verify_by,
            DateTime::from_timestamp(1_800_000_000 + 10 * 86_400, 0)
        );
        let json = serde_json::to_value(&saved).unwrap();
        assert_eq!(json["type"], "us_bank_account");
        assert_eq!(json["status"], "verifying");
    }

    #[test]
    fn only_succeeded_or_verifying_setups_are_saved() {
        let ready = saved_method(&bank_setup("succeeded")).unwrap();
        assert_eq!(ready.status, SavedMethodStatus::Ready);
        assert_eq!((ready.verify_url, ready.verify_by), (None, None));
        assert!(saved_method(&bank_setup("requires_payment_method")).is_none());
        let mut other = bank_setup("requires_action");
        other["next_action"]["type"] = json!("redirect_to_url");
        assert!(saved_method(&other).is_none());
        let card = json!({
            "status": "succeeded",
            "payment_method": { "id": "pm_1", "type": "card",
                                "card": { "brand": "visa", "last4": "4242" } },
        });
        let card = saved_method(&card).unwrap();
        assert_eq!(
            (
                card.method_type,
                card.brand.as_deref(),
                card.last4.as_deref()
            ),
            (PaymentMethodType::Card, Some("visa"), Some("4242"))
        );
        let link =
            json!({ "status": "succeeded", "payment_method": { "id": "pm_2", "type": "link" } });
        assert!(saved_method(&link).is_none());
        let unexpanded = json!({ "status": "succeeded", "payment_method": "pm_3" });
        assert!(saved_method(&unexpanded).is_none());
    }

    #[test]
    fn payment_facts_record_the_mandate_and_sec_code_of_bank_accounts() {
        let facts = payment_facts(Some("cus_1".into()), &bank_setup("succeeded")).unwrap();
        assert_eq!(facts.payment_method_id.as_deref(), Some("pm_bank"));
        assert_eq!(facts.method_type, Some(PaymentMethodType::UsBankAccount));
        assert_eq!(facts.bank_name.as_deref(), Some("STRIPE TEST BANK"));
        assert_eq!(facts.mandate_id.as_deref(), Some("mandate_1"));
        assert_eq!(facts.sec_code, Some("CCD"));
        let mut personal = bank_setup("succeeded");
        personal["mandate"] = json!("mandate_2");
        personal["payment_method"]["us_bank_account"]["account_holder_type"] = json!("individual");
        let facts = payment_facts(None, &personal).unwrap();
        assert_eq!(
            (facts.mandate_id.as_deref(), facts.sec_code),
            (Some("mandate_2"), Some("WEB"))
        );
        let card = json!({
            "status": "succeeded", "mandate": "mandate_3",
            "payment_method": { "id": "pm_1", "type": "card",
                                "card": { "brand": "visa", "last4": "4242" } },
        });
        let facts = payment_facts(None, &card).unwrap();
        assert_eq!(
            (facts.bank_name, facts.mandate_id, facts.sec_code),
            (None, None, None)
        );
    }
}
