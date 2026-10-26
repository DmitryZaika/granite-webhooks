//! The payments app's account page: the subscription and its add-ons.
//!
//! - `GET /v1/billing/subscription`  → plan, users, add-ons, next invoice, payment method, invoices (Managers)
//! - `PUT /v1/billing/addons/{key}`  → add or remove an add-on, prorated (Managers)

use crate::auth::BillingManager;
use crate::billing::addons::{self, AccountSubscription, AddonError};
use crate::billing::lock::BUSY_MESSAGE;
use crate::billing::reactivation::NOT_CONFIGURED;
use crate::billing::stripe::StripeError;
use crate::entitlements::AddonKey;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use lambda_http::tracing;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(account_subscription))
        .routes(routes!(set_addon))
}

impl From<AddonError> for ApiError {
    fn from(err: AddonError) -> Self {
        match err {
            AddonError::Refused(message) => Self::Conflict(message),
            AddonError::Busy => Self::Unavailable(BUSY_MESSAGE.into()),
            AddonError::Database(err) => Self::Database(err),
            AddonError::Stripe(StripeError::NotConfigured) => {
                Self::Unavailable(NOT_CONFIGURED.into())
            }
            AddonError::Stripe(StripeError::Api(failure)) => Self::BadRequest(failure.message),
            AddonError::Stripe(_) => {
                Self::Upstream("Stripe could not be reached. Please try again.".into())
            }
            AddonError::Catalog(_) => {
                Self::Unavailable("Prices are not available right now. Please try again.".into())
            }
        }
    }
}

/// Get the subscription for the account page
///
/// The company's self-serve subscription as the payments app's account page
/// shows it: status and free period, the base plan, billable users, each
/// add-on (`active`, `available`, or `included` free by Granite Manager) with
/// the prorated estimate of changing it now, the monthly total, Stripe's
/// draft of the next invoice, the payment method on file (brand or bank and
/// last four digits only), a scheduled cancellation and the latest 12
/// invoices. Read live from Stripe, each read within 5 seconds; when Stripe
/// cannot be reached the stored subscription and add-ons are shown,
/// `next_invoice` and `payment_method` are null, and prices Stripe alone
/// knows are null (`can_cancel` still holds). A legacy company (`plan_source`
/// `legacy`, billed by Granite Manager) gets only its name and plan source:
/// it manages billing on `/admin/billing` (`GET /v1/billing`). Change an
/// add-on with `PUT /v1/billing/addons/{key}`; open the card form with
/// `POST /v1/billing/portal` (`return_to: "account"`). Managers only.
#[utoipa::path(
    get,
    path = "/v1/billing/subscription",
    operation_id = "get_billing_subscription",
    tag = "billing",
    responses(
        (status = 200, description = "The subscription", body = AccountSubscription),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn account_subscription(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
) -> ApiResult<Json<AccountSubscription>> {
    let view = Box::pin(addons::account_view(&state, user.company_id)).await?;
    Ok(Json(view))
}

/// Body of `PUT /v1/billing/addons/{key}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AddonChange {
    /// `true` adds the add-on to the subscription, `false` removes it.
    pub enabled: bool,
}

/// Add or remove an add-on
///
/// Adds the add-on to the company's subscription (`enabled` true) or removes
/// it (false), prorated: adding charges the rest of the current period on
/// the next invoice (nothing during the free period), removing credits it.
/// The add-on is usable (or gone) at once. Already in the asked state:
/// nothing changes. Answers the account page as
/// `GET /v1/billing/subscription` does, after the change. 409 when the plan
/// is legacy, there is no running subscription, the add-on is included free
/// by Granite Manager, or an add-on is added while a payment is overdue (pay
/// the open invoice first). Managers only.
#[utoipa::path(
    put,
    path = "/v1/billing/addons/{key}",
    operation_id = "set_billing_addon",
    tag = "billing",
    params(("key" = String, Path, description = "`signatures`, `quotes` or `quickbooks`")),
    request_body = AddonChange,
    responses(
        (status = 200, description = "The subscription after the change", body = AccountSubscription),
        (status = 400, description = "Stripe refused the change, or the body is malformed", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a manager of the company", body = ErrorBody),
        (status = 404, description = "Unknown add-on", body = ErrorBody),
        (status = 409, description = "Legacy plan, no running subscription, included free, or a payment is overdue", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Stripe could not be reached", body = ErrorBody),
        (status = 503, description = "Billing is busy or not set up, or prices are unavailable; try again", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_addon(
    State(state): State<AppState>,
    BillingManager(user): BillingManager,
    Path(key): Path<String>,
    JsonBody(input): JsonBody<AddonChange>,
) -> ApiResult<Json<AccountSubscription>> {
    let addon = AddonKey::parse(&key).ok_or(ApiError::NotFound)?;
    if let Err(err) = addons::set_addon(&state, user.company_id, addon, input.enabled).await {
        if !matches!(err, AddonError::Refused(_)) {
            tracing::warn!(
                company_id = user.company_id,
                addon = addon.as_str(),
                error = %err.reason(),
                "add-on change failed"
            );
        }
        return Err(err.into());
    }
    let view = Box::pin(addons::account_view(&state, user.company_id)).await?;
    Ok(Json(view))
}
