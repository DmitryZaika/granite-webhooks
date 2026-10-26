//! Cancellation requests from the payments app's account page (step 10).
//!
//! - `POST   /v1/billing/cancellation-request` → "Request cancellation" (Managers)
//! - `DELETE /v1/billing/cancellation-request` → "Keep my subscription" (Managers)

use crate::auth::BillingManager;
use crate::billing::cancellation::{self, CancellationError, CancellationRequest, Requested};
use crate::billing::lock::{BUSY_MESSAGE, LockError};
use crate::billing::stripe::StripeError;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(
        request_billing_cancellation,
        withdraw_billing_cancellation
    ))
}

impl From<CancellationError> for ApiError {
    fn from(err: CancellationError) -> Self {
        match err {
            CancellationError::Invalid(message) => Self::BadRequest(message),
            CancellationError::Conflict(message) => Self::Conflict(message),
            CancellationError::Lock(LockError::Busy) => Self::Unavailable(BUSY_MESSAGE.into()),
            CancellationError::Lock(LockError::Database(err))
            | CancellationError::Database(err) => Self::Database(err),
            CancellationError::Stripe(StripeError::NotConfigured) => {
                Self::Unavailable("Billing is not available right now.".into())
            }
            CancellationError::Stripe(err) => {
                tracing::warn!(code = err.code(), "cancellation Stripe call failed");
                Self::Upstream("We could not reach the payment service. Please try again.".into())
            }
        }
    }
}

/// Body of `POST /v1/billing/cancellation-request`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct CancellationInput {
    /// Why the company is leaving, optional, at most 1000 characters. Trimmed;
    /// empty counts as none. Only Granite Manager staff see it.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The company's scheduled cancellation request.
#[derive(Debug, Serialize, ToSchema)]
pub struct CancellationRequestBody {
    /// Request id.
    pub id: i64,
    /// When it was requested (ISO 8601 UTC).
    pub requested_at: DateTime<Utc>,
    /// When the subscription ends (ISO 8601 UTC): the end of the free period
    /// when requested during it (nothing is charged then), else the end of
    /// the current paid month. Everything keeps working until then.
    pub effective_at: DateTime<Utc>,
    /// Name of the Manager who requested it (their email when they have no
    /// name).
    pub requested_by: String,
    /// The reason given, or null.
    pub reason: Option<String>,
}

impl From<CancellationRequest> for CancellationRequestBody {
    fn from(request: CancellationRequest) -> Self {
        Self {
            id: request.id,
            requested_at: request.requested_at.and_utc(),
            effective_at: request.effective_at.and_utc(),
            requested_by: request.requested_by_name,
            reason: request.reason,
        }
    }
}

/// Request cancellation of the subscription
///
/// "Request cancellation" on the account page, for a Manager of a self-serve
/// company with a running subscription (trialing, active or past due). The
/// subscription ends at the end of the free period when requested during it
/// (then nothing is charged), else at the end of the current paid month; the
/// company keeps full access until then. Answers 201 with the new request,
/// and emails a confirmation to every Manager and a notice to Granite Manager
/// staff (with the reason). When a request is already scheduled, answers 200
/// with it and changes nothing (no second email). Withdraw it before it takes
/// effect with `DELETE /v1/billing/cancellation-request`; `GET
/// /v1/billing/subscription` shows it as `cancellation`.
#[utoipa::path(
    post,
    path = "/v1/billing/cancellation-request",
    operation_id = "request_billing_cancellation",
    tag = "billing",
    request_body = CancellationInput,
    responses(
        (status = 201, description = "Requested", body = CancellationRequestBody),
        (status = 200, description = "Already scheduled: the existing request, unchanged", body = CancellationRequestBody),
        (status = 400, description = "Reason longer than 1000 characters, or a malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 409, description = "Plan managed by Granite Manager staff (legacy), or no running subscription", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe failed; try again", body = ErrorBody),
        (status = 503, description = "Billing busy or not configured; try again", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn request_billing_cancellation(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    JsonBody(input): JsonBody<CancellationInput>,
) -> ApiResult<(StatusCode, Json<CancellationRequestBody>)> {
    let outcome =
        cancellation::request(&state, user.company_id, user.id, input.reason.as_deref()).await?;
    Ok(match outcome {
        Requested::Created { request, .. } => (StatusCode::CREATED, Json(request.into())),
        Requested::Existing(request) => (StatusCode::OK, Json(request.into())),
    })
}

/// Withdraw the cancellation request
///
/// "Keep my subscription" on the account page, for a Manager: the
/// subscription renews again as before and every Manager and Granite Manager
/// staff are emailed. Only while the request is scheduled and before it takes
/// effect. A repeat (the latest request is already withdrawn) answers 204 and
/// changes nothing (no second email); with no request ever made, or after the
/// cancellation took effect, answers 409.
#[utoipa::path(
    delete,
    path = "/v1/billing/cancellation-request",
    operation_id = "withdraw_billing_cancellation",
    tag = "billing",
    responses(
        (status = 204, description = "Withdrawn, or already withdrawn (nothing changed)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a Manager of the company", body = ErrorBody),
        (status = 409, description = "No request ever made, already taken effect, legacy plan, or no running subscription", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe failed; try again", body = ErrorBody),
        (status = 503, description = "Billing busy or not configured; try again", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn withdraw_billing_cancellation(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<StatusCode> {
    cancellation::withdraw(&state, user.company_id, user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
