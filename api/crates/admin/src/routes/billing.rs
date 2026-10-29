//! The company's Stripe subscription.
//!
//! - `POST /v1/billing/reactivation`        → start paying to restore a suspended company
//! - `POST /v1/billing/reactivation/finish` → after the browser confirmed the payment
//! - `POST /v1/webhooks/stripe`             → Stripe's events (signed, no session)
//!
//! Replaces the Remix `api.billing.reactivate` and `api.stripe.webhook`
//! routes. The billing page itself is `billing_account`.

use crate::auth::BillingManager;
use crate::billing::gate::billing_gate;
use crate::billing::items::sync_items;
use crate::billing::lock::CompanyLock;
use crate::billing::machine::{apply_event, event_company_id};
use crate::billing::reactivation::{self, Started};
use crate::billing::store::{BillingStore, Claim, MySqlBillingStore};
use crate::billing::stripe::{latest_event, normalize_event, verify_signature};
use crate::billing::{self, BillingEvent, PaymentStatus, SubscriptionSnapshot};
use crate::error::{ApiError, ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start_reactivation))
        .routes(routes!(finish_reactivation))
        .routes(routes!(stripe_webhook))
        .merge(super::billing_methods::router())
}

/// Body of `POST /v1/billing/reactivation`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StartReactivation {
    /// The Stripe card to pay with (`pm_...`): a saved card of the company,
    /// or one Stripe.js just created.
    pub payment_method_id: String,
}

/// Where the payment stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartStatus {
    /// The company is paid up: nothing to pay, access is back.
    Paid,
    /// Confirm the payment in the browser, then finish.
    RequiresPayment,
}

/// How the payment goes on.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReactivationStarted {
    /// `paid`, or `requires_payment`: confirm the payment in the browser
    /// (`stripe.confirmCardPayment`) with `client_secret`, then call
    /// `/v1/billing/reactivation/finish` with `subscription_id`.
    pub status: StartStatus,
    /// The first invoice's payment confirmation secret; absent when paid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// The new subscription, for the finish call; absent when paid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
}

const PAID: ReactivationStarted = ReactivationStarted {
    status: StartStatus::Paid,
    client_secret: None,
    subscription_id: None,
};

/// Start paying to restore access
///
/// For a company whose subscription ended unpaid: creates a new subscription
/// charged to the card (managers only). Answers `paid` when there is nothing
/// to pay (the company is not restricted, or Stripe already has it paid),
/// otherwise the payment for the browser to confirm. A retry for the same
/// plan confirms the first payment still open instead of creating another
/// subscription. Stripe's refusals ("Your card was declined.") are 400s
/// with Stripe's message. A self-serve company is refused with 400 ("This
/// company is on a self-serve plan. Contact Granite Manager to start
/// again."): it starts again through a plan invite.
#[crate::openapi::path(
    post,
    path = "/v1/billing/reactivation",
    operation_id = "start_billing_reactivation",
    tag = "billing",
    request_body = StartReactivation,
    responses(
        (status = 200, description = "Paid, or a payment to confirm", body = ReactivationStarted),
        (status = 400, description = "No card, Stripe refused, a self-serve company, or another payment of the company is in progress", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start_reactivation(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<StartReactivation>,
) -> ApiResult<Json<ReactivationStarted>> {
    let gate = Box::pin(billing_gate(
        &state.stripe,
        &state.pool,
        user.company_id,
        user.id,
    ))
    .await;
    if !gate.restricted {
        return Ok(Json(PAID));
    }
    let payment_method_id = input.payment_method_id.trim();
    if !payment_method_id.starts_with("pm_") {
        return Err(ApiError::BadRequest("Choose a card to pay with".into()));
    }
    let started = reactivation::start(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        &state.billing_plans,
        user.company_id,
        payment_method_id,
    )
    .await?;
    Ok(Json(match started {
        Started::Paid => PAID,
        Started::RequiresPayment {
            client_secret,
            subscription_id,
        } => ReactivationStarted {
            status: StartStatus::RequiresPayment,
            client_secret: Some(client_secret),
            subscription_id: Some(subscription_id),
        },
    }))
}

/// Body of `POST /v1/billing/reactivation/finish`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FinishReactivation {
    /// The subscription the start call created (`sub_...`); omit when it
    /// answered `paid`.
    #[serde(default)]
    pub subscription_id: Option<String>,
}

/// Whether access is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReactivationStatus {
    /// Paid: the company has access again.
    Active,
    /// Stripe has not confirmed the payment yet; try again shortly.
    Pending,
}

/// Body of the finish answer.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReactivationFinished {
    /// `active` or `pending`.
    pub status: ReactivationStatus,
}

/// Finish restoring access
///
/// After the browser confirmed the payment: waits a few seconds for Stripe
/// to mark the subscription paid and applies it.
#[crate::openapi::path(
    post,
    path = "/v1/billing/reactivation/finish",
    operation_id = "finish_billing_reactivation",
    tag = "billing",
    request_body = FinishReactivation,
    responses(
        (status = 200, description = "Active or pending", body = ReactivationFinished),
        (status = 400, description = "Not the company's subscription, or Stripe failed", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish_reactivation(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<FinishReactivation>,
) -> ApiResult<Json<ReactivationFinished>> {
    let subscription_id = input
        .subscription_id
        .as_deref()
        .map(str::trim)
        .filter(|id| id.starts_with("sub_"));
    let status = reactivation::finish(
        &state.stripe,
        &MySqlBillingStore(&state.pool),
        user.company_id,
        subscription_id,
    )
    .await?;
    Ok(Json(ReactivationFinished {
        status: if status == PaymentStatus::Active {
            ReactivationStatus::Active
        } else {
            ReactivationStatus::Pending
        },
    }))
}

/// Answer to Stripe.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebhookReceived {
    /// Always true.
    pub received: bool,
}

/// Receive a Stripe event
///
/// Stripe's webhook endpoint: checkout, subscription, invoice and payment
/// method events update the company's billing (each event once); a plan
/// invite's subscription (`metadata.plan_invite_id`) only once the invite
/// is accepted. Card and bank account events (`setup_intent.succeeded`,
/// `setup_intent.setup_failed`, `payment_intent.processing`,
/// `charge.failed`, `charge.dispute.created`,
/// `payment_method.automatically_updated`) are re-read from Stripe: a
/// verified bank account becomes the default (or completes the signup
/// waiting on it), a failed one is reported to the Managers, a bank payment
/// in flight keeps access on, a blocked account or a dispute starts a 7-day
/// hold. A paid
/// subscription invoice that carried the onboarding advance (its invoice
/// item, `metadata.kind = onboarding_advance`) credits that month back to
/// the customer's balance, once. Needs a valid `Stripe-Signature` for
/// `STRIPE_WEBHOOK_SECRET`; no session.
#[crate::openapi::path(
    post,
    path = "/v1/webhooks/stripe",
    operation_id = "receive_stripe_webhook",
    tag = "billing",
    request_body(content = Object, description = "The Stripe event, as Stripe sent it"),
    params(("Stripe-Signature" = String, Header, description = "Stripe's signature of the body")),
    responses(
        (status = 200, description = "Processed (or ignored)", body = WebhookReceived),
        (status = 400, description = "Missing or invalid signature, or not configured", body = ErrorBody),
        (status = 500, description = "Processing failed; Stripe retries", body = ErrorBody),
        (status = 409, description = "`event_in_progress`: another delivery of the event is still being processed (or stopped less than 5 minutes ago); Stripe retries", body = CodedErrorBody),
    )
)]
pub async fn stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<WebhookReceived>> {
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok());
    let (Some(signature), Some(secret)) = (signature, state.stripe_webhook_secret.as_deref())
    else {
        return Err(ApiError::BadRequest("Webhook is not configured".into()));
    };
    if let Err(err) = verify_signature(&body, signature, secret, chrono::Utc::now().timestamp()) {
        tracing::warn!(error = %err, "Stripe webhook signature refused");
        return Err(ApiError::BadRequest("Invalid webhook signature".into()));
    }
    let event: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Invalid webhook payload".into()))?;
    if crate::billing::methods::is_method_event(&event) {
        // Card and bank payment methods: claimed once, re-read from Stripe,
        // never under the company lock (signup provisioning takes its own).
        return match crate::billing::methods::apply_method_event(&state, &event).await {
            Ok(Claim::InProgress) => Err(event_in_progress()),
            Ok(_) => Ok(Json(WebhookReceived { received: true })),
            Err(err) => {
                tracing::error!(error = %err, "Stripe payment method webhook failed");
                Err(ApiError::Internal("Webhook processing failed".into()))
            }
        };
    }
    let Some(normalized) = normalize_event(&event) else {
        return Ok(Json(WebhookReceived { received: true }));
    };
    let result = async {
        let store = MySqlBillingStore(&state.pool);
        // The payload's ids name the company; the lock comes before the read
        // of Stripe's latest state, so a write applied meanwhile under the
        // lock (an add-on, a cancellation) is never overwritten by an older
        // read.
        let company_id = event_company_id(&store, &normalized)
            .await
            .map_err(|err| err.to_string())?;
        let lock = match company_id {
            Some(company_id) => Some(
                CompanyLock::acquire(&state.pool, company_id)
                    .await
                    .map_err(|err| err.to_string())?,
            ),
            None => None,
        };
        let applied = async {
            let Some(latest) = latest_event(&state.stripe, normalized)
                .await
                .map_err(|err| err.to_string())?
            else {
                return Ok(Claim::Done);
            };
            let claim = apply_event(&store, &latest, billing::now())
                .await
                .map_err(|err| err.to_string())?
                .claim;
            if claim == Claim::InProgress {
                // The other delivery does the rest.
                return Ok(claim);
            }
            if let (Some(company_id), BillingEvent::Subscription { subscription, .. }) =
                (company_id, &latest)
            {
                sync_subscription_items(&state, &store, company_id, subscription).await?;
            }
            // A paid or failed invoice ends the bank payment in flight.
            if let (Some(company_id), BillingEvent::Invoice { .. }) = (company_id, &latest) {
                crate::billing::methods::clear_processing(&state.pool, company_id)
                    .await
                    .map_err(|err| err.to_string())?;
            }
            // A paid invoice carrying the onboarding advance credits it back.
            if let (Some(company_id), BillingEvent::Invoice { paid: true, .. }) =
                (company_id, &latest)
            {
                crate::onboarding_call::advance::invoice_paid(
                    &state,
                    company_id,
                    &event["data"]["object"],
                    billing::now(),
                )
                .await
                .map_err(|err| err.code())?;
            }
            Ok::<Claim, String>(claim)
        }
        .await;
        if let Some(lock) = lock {
            lock.release().await;
        }
        applied
    }
    .await;
    match result {
        Ok(Claim::InProgress) => Err(event_in_progress()),
        Ok(_) => Ok(Json(WebhookReceived { received: true })),
        Err(err) => {
            tracing::error!(error = %err, "Stripe webhook processing failed");
            Err(ApiError::Internal("Webhook processing failed".into()))
        }
    }
}

/// Another delivery of the event holds its claim: a 503 Stripe retries
/// (a 2xx would end Stripe's retries, and the event could be lost).
fn event_in_progress() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "event_in_progress",
        "This event is still being processed. Retry later.",
    )
}

/// Syncs a subscription event's items into the company's add-ons, when it is
/// the company's current subscription (an old one ending changes nothing).
/// Runs for a repeated delivery too: the sync is idempotent, and a retry
/// after a failed sync must not be lost to the event's claim.
async fn sync_subscription_items(
    state: &AppState,
    store: &MySqlBillingStore<'_>,
    company_id: i32,
    snapshot: &SubscriptionSnapshot,
) -> Result<(), String> {
    let current = store
        .by_company(company_id)
        .await
        .map_err(|err| err.to_string())?;
    if current
        .and_then(|record| record.stripe_subscription_id)
        .as_deref()
        != Some(&snapshot.id)
    {
        return Ok(());
    }
    sync_items(&state.pool, company_id, snapshot, billing::now())
        .await
        .map_err(|err| err.to_string())
}
