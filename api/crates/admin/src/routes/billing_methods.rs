//! Card and bank account payment methods on the payments app (step 13).
//!
//! - `POST /v1/billing/payment-setup`   → a SetupIntent for the Payment Element (Managers)
//! - `POST /v1/billing/payment-method`  → apply a confirmed SetupIntent: the new default (Managers)
//! - `POST /v1/billing/invoice-payment` → pay the oldest open invoice with the saved method (Managers)
//!
//! Open while the company's access is paused (`/v1/billing*`). See
//! `crate::billing::methods`.

use crate::auth::{BillingManager, PositionedUser};
use crate::billing::MANAGER_POSITION;
use crate::billing::addons::AccountPaymentMethod;
use crate::billing::invites::accept::{self, Caller};
use crate::billing::methods::{self, Applied, InvoicePayment, SetupFor};
use crate::error::{ApiError, ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::routes::signup::schemas::{PaymentMode, PaymentSetup};
use crate::signup::payment::Setup;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_payment_setup))
        .routes(routes!(apply_payment_method))
        .routes(routes!(pay_open_invoice))
}

/// Body of `POST /v1/billing/payment-setup`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PaymentSetupRequest {
    /// `update` (the account page: replace the saved method), `pay` (the pay
    /// page: a method to pay an overdue invoice with) or `plan_invite` (the
    /// upgrade page, with `plan_invite_token`).
    pub purpose: String,
    /// The token of the `/upgrade/<token>` link; only for `plan_invite`.
    #[serde(default)]
    pub plan_invite_token: Option<String>,
}

fn payment_setup(setup: Setup) -> PaymentSetup {
    match setup {
        Setup::Test => PaymentSetup {
            mode: PaymentMode::Test,
            client_secret: None,
            publishable_key: None,
        },
        Setup::Stripe {
            client_secret,
            publishable_key,
        } => PaymentSetup {
            mode: PaymentMode::Stripe,
            client_secret: Some(client_secret),
            publishable_key: Some(publishable_key),
        },
    }
}

/// Set up a card or bank account
///
/// Creates a SetupIntent (card or US bank account, saved for later charges)
/// on the company's Stripe customer and answers `{"mode": "stripe",
/// "client_secret", "publishable_key"}` for Stripe's Payment Element; after
/// `stripe.confirmSetup` call `apply_billing_payment_method` with the
/// SetupIntent id. In the sandbox seam (a test key with a test payment
/// method configured) `update` and `pay` save a test card here and answer
/// `{"mode": "test"}`. `plan_invite` sets up the method of the open plan
/// invite behind `plan_invite_token` (the caller must be a Manager of the
/// invited company). `update` and `pay` need a Manager of the company the
/// session acts in. Nothing is charged.
#[crate::openapi::path(
    post,
    path = "/v1/billing/payment-setup",
    operation_id = "create_billing_payment_setup",
    tag = "billing",
    request_body = PaymentSetupRequest,
    responses(
        (status = 200, description = "How to collect the payment method", body = PaymentSetup),
        (status = 400, description = "Unknown purpose, or `plan_invite` without a token", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company (or of the invited company: code not_invited)", body = ErrorBody),
        (status = 404, description = "Unknown or closed plan invite (code invite_not_found)", body = CodedErrorBody),
        (status = 409, description = "The company has no Stripe customer (code no_customer)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe refused the setup", body = ErrorBody),
        (status = 503, description = "Stripe cannot be reached or payments are not configured (code payments_unavailable)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_payment_setup(
    State(state): State<AppState>,
    PositionedUser(user, positions): PositionedUser,
    JsonBody(input): JsonBody<PaymentSetupRequest>,
) -> ApiResult<Json<PaymentSetup>> {
    let purpose = match input.purpose.trim() {
        "update" => SetupFor::Update,
        "pay" => SetupFor::Pay,
        "plan_invite" => {
            let token = input
                .plan_invite_token
                .as_deref()
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| ApiError::BadRequest("plan_invite_token is required".into()))?;
            let caller = Caller {
                user: &user,
                positions: &positions,
            };
            let setup = accept::create_setup(&state, token, caller).await?;
            return Ok(Json(payment_setup(setup)));
        }
        _ => {
            return Err(ApiError::BadRequest(
                "purpose must be update, pay or plan_invite".into(),
            ));
        }
    };
    let manager = positions
        .iter()
        .any(|row| row.company_id == user.company_id && row.position_id == MANAGER_POSITION);
    if !(manager && (user.is_employee || user.is_admin || user.is_superuser)) {
        return Err(ApiError::Forbidden);
    }
    let setup = methods::company_setup(&state, user.company_id, purpose).await?;
    Ok(Json(payment_setup(setup)))
}

/// Body of `POST /v1/billing/payment-method`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ApplyPaymentMethod {
    /// The SetupIntent the Payment Element confirmed (`seti_...`).
    pub setup_intent_id: String,
}

/// What became of the payment method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppliedStatus {
    /// It is the company's payment method now; the previous one was removed.
    Saved,
    /// A bank account waiting for its micro-deposits (1 to 2 business
    /// days): verify it at `verify_url` by `verify_by`. The current method
    /// stays until then.
    Verifying,
    /// Not usable (declined, cancelled or not confirmed): nothing changed.
    Failed,
}

/// Answer of `POST /v1/billing/payment-method`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaymentMethodApplied {
    pub status: AppliedStatus,
    /// The card or bank account (brand or bank and last four digits); null
    /// when `failed` or Stripe did not say.
    pub payment_method: Option<AccountPaymentMethod>,
    /// Stripe's page to enter the micro-deposit code; only while `verifying`.
    pub verify_url: Option<String>,
    /// The bank account must be verified by then (ISO 8601 UTC); only while
    /// `verifying`.
    pub verify_by: Option<DateTime<Utc>>,
}

/// Apply a confirmed payment method
///
/// Reads the SetupIntent from Stripe (the browser's word is never trusted):
/// it must be the company's (its `metadata.company_id` and customer), else
/// 404 `setup_not_found`. `succeeded`: it becomes the company's payment
/// method (the customer's and the subscription's default), the old one is
/// removed and a hold on the old method ends (`saved`); a company whose
/// signup waits for its bank account gets its subscription now.
/// Micro-deposits pending: kept until Stripe verifies the account
/// (`verifying`, the current method stays). Anything else: `failed`.
/// Calling it again with the same SetupIntent gives the same answer. Then
/// call `pay_billing_open_invoice` on the pay page.
#[crate::openapi::path(
    post,
    path = "/v1/billing/payment-method",
    operation_id = "apply_billing_payment_method",
    tag = "billing",
    request_body = ApplyPaymentMethod,
    responses(
        (status = 200, description = "Saved, verifying or failed", body = PaymentMethodApplied),
        (status = 400, description = "Not a SetupIntent id", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 404, description = "Not a SetupIntent of the company (code setup_not_found)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe refused a change", body = ErrorBody),
        (status = 503, description = "Stripe cannot be reached (code payments_unavailable), or billing is busy", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn apply_payment_method(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<ApplyPaymentMethod>,
) -> ApiResult<Json<PaymentMethodApplied>> {
    let id = input.setup_intent_id.trim();
    if !id.starts_with("seti_") {
        return Err(ApiError::BadRequest("Not a payment method setup".into()));
    }
    let setup_intent = match state.stripe.setup_intent(id).await {
        Ok(setup_intent) => setup_intent,
        Err(crate::billing::stripe::StripeError::Missing(_)) => {
            return Err(methods::setup_not_found());
        }
        Err(err) => return Err(methods::stripe_failure(&err, user.company_id, "read setup")),
    };
    let applied = methods::apply_setup(&state, user.company_id, &setup_intent).await?;
    Ok(Json(match applied {
        Applied::Saved { payment_method } => PaymentMethodApplied {
            status: AppliedStatus::Saved,
            payment_method,
            verify_url: None,
            verify_by: None,
        },
        Applied::Verifying {
            payment_method,
            verify_url,
            verify_by,
        } => PaymentMethodApplied {
            status: AppliedStatus::Verifying,
            payment_method,
            verify_url,
            verify_by: verify_by.map(|at| at.and_utc()),
        },
        Applied::Failed => PaymentMethodApplied {
            status: AppliedStatus::Failed,
            payment_method: None,
            verify_url: None,
            verify_by: None,
        },
    }))
}

/// Body of `POST /v1/billing/invoice-payment`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvoicePaymentRequest {
    /// A UUID the page makes once per "Pay" press: a retry of the same press
    /// never charges twice.
    pub attempt_id: String,
}

/// How the payment went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvoicePaymentStatus {
    /// Paid by card. Access comes back once Stripe confirms (moments later).
    Paid,
    /// A bank payment is on its way (up to 4 business days); access stays on.
    Processing,
    /// The card needs 3D Secure: `stripe.handleNextAction({clientSecret})`.
    RequiresAction,
    /// The card was declined: show `decline_message`, offer another method.
    Declined,
    /// No open invoice: nothing to pay.
    NothingDue,
}

/// Answer of `POST /v1/billing/invoice-payment`.
#[derive(Serialize, ToSchema)]
pub struct InvoicePaymentResult {
    pub status: InvoicePaymentStatus,
    /// The invoice's confirmation secret; only for `requires_action`.
    pub client_secret: Option<String>,
    /// Stripe's decline message, to show as is; only for `declined`.
    pub decline_message: Option<String>,
    /// The amount of the invoice paid (or tried), in cents; null for
    /// `nothing_due`.
    pub amount_cents: Option<i64>,
}

/// Pay the open invoice
///
/// Pays the company's oldest open subscription invoice (never the onboarding
/// fee) with its saved payment method. Never restores access by itself:
/// Stripe's confirmation does (`GET /v1/billing/gate` shows it moments
/// later for a card, after up to 4 business days for a bank account, which
/// keeps access on meanwhile). `attempt_id` makes a retry of the same press
/// safe. 409 `no_payment_method` when none is saved: set one up first with
/// `create_billing_payment_setup` (`purpose: "pay"`).
#[crate::openapi::path(
    post,
    path = "/v1/billing/invoice-payment",
    operation_id = "pay_billing_open_invoice",
    tag = "billing",
    request_body = InvoicePaymentRequest,
    responses(
        (status = 200, description = "Paid, processing, needs 3D Secure, declined, or nothing due", body = InvoicePaymentResult),
        (status = 400, description = "attempt_id is not a UUID", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 409, description = "No payment method saved (code no_payment_method)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe refused the payment for another reason", body = ErrorBody),
        (status = 503, description = "Stripe cannot be reached (code payments_unavailable)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn pay_open_invoice(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<InvoicePaymentRequest>,
) -> ApiResult<Json<InvoicePaymentResult>> {
    let result = |status, amount_cents: Option<i64>| InvoicePaymentResult {
        status,
        client_secret: None,
        decline_message: None,
        amount_cents,
    };
    Ok(Json(
        match methods::pay_open_invoice(&state, user.company_id, &input.attempt_id).await? {
            InvoicePayment::Paid { amount_cents } => {
                result(InvoicePaymentStatus::Paid, Some(amount_cents))
            }
            InvoicePayment::Processing { amount_cents } => {
                result(InvoicePaymentStatus::Processing, Some(amount_cents))
            }
            InvoicePayment::RequiresAction {
                client_secret,
                amount_cents,
            } => InvoicePaymentResult {
                client_secret: Some(client_secret),
                ..result(InvoicePaymentStatus::RequiresAction, Some(amount_cents))
            },
            InvoicePayment::Declined {
                message,
                amount_cents,
            } => InvoicePaymentResult {
                decline_message: Some(message),
                ..result(InvoicePaymentStatus::Declined, Some(amount_cents))
            },
            InvoicePayment::NothingDue => result(InvoicePaymentStatus::NothingDue, None),
        },
    ))
}
