//! A Manager's side of "Move to plan" (the payments app's `/upgrade/<token>`).
//!
//! - `GET  /v1/plan-invites/{token}`            → the plan, its price and the consents
//! - `POST /v1/plan-invites/{token}/acceptance` → accept: the company moves onto the plan
//!
//! Session routes: only a Manager of the invited company opens them (their
//! session may act in another company). Open while the company's access is
//! paused. The token is a bearer secret. See `crate::billing::invites`.

use crate::auth::PositionedUser;
use crate::billing::invites::accept::{self, Caller, InviteOffer};
use crate::error::{ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::signup::RequestMeta;
use crate::signup::consent::ConsentAcceptance;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_plan_invite))
        .routes(routes!(accept_plan_invite))
}

/// Read a plan invite
///
/// The plan Granite Manager set up for the caller's company: the add-ons,
/// the price for its active users now (`quote`), the first charge, the
/// legacy plan it replaces (`current_plan`), when the link expires, and the
/// checkboxes to tick (`consents`, rendered by the server: show `text`
/// verbatim and send `kind`, `version` and `sha256` back to
/// `accept_plan_invite`). `status` `accepting` means an acceptance is being
/// finished (call `accept_plan_invite` again to resume); `accepted` means
/// the company is on the plan (go to `redirect_url`). Only a Manager of the
/// invited company, whatever company the session acts in: 403
/// `not_invited` otherwise.
#[crate::openapi::path(
    get,
    path = "/v1/plan-invites/{token}",
    operation_id = "get_plan_invite",
    tag = "billing",
    params(("token" = String, Path, description = "The token of the emailed `/upgrade/<token>` link")),
    responses(
        (status = 200, description = "The invite", body = InviteOffer),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a Manager of the invited company (code not_invited)", body = CodedErrorBody),
        (status = 404, description = "Unknown link (code invite_not_found)", body = CodedErrorBody),
        (status = 410, description = "The invite expired or was withdrawn (codes invite_expired, invite_revoked)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Prices cannot be loaded now (code catalog_unavailable)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_plan_invite(
    State(state): State<AppState>,
    PositionedUser(user, positions): PositionedUser,
    Path(token): Path<String>,
) -> ApiResult<Json<InviteOffer>> {
    let caller = Caller {
        user: &user,
        positions: &positions,
    };
    Ok(Json(accept::offer(&state, &token, caller).await?))
}

/// Body of `POST /v1/plan-invites/{token}/acceptance`.
#[derive(Deserialize, ToSchema)]
pub struct AcceptanceBody {
    /// Every checkbox the Manager ticked, as `get_plan_invite` showed it.
    pub consents: Vec<ConsentAcceptance>,
}

/// Answer of `POST /v1/plan-invites/{token}/acceptance`.
#[derive(Debug, Serialize, ToSchema)]
pub struct AcceptanceDone {
    /// Always `accepted`: the company is on the plan.
    pub status: String,
    /// Where the browser goes next (the main app's billing page).
    pub redirect_url: String,
}

/// Accept a plan invite
///
/// Sends the checkboxes the Manager ticked (`kind`, `version`,
/// `text_sha256` as `get_plan_invite` returned them); the server renders
/// them again and refuses with 409 `consents_changed` when anything differs
/// (read the invite again). Then it sets up the payment method, makes the
/// company's Stripe subscription with nothing charged before the first
/// charge, ends a legacy subscription at its period end, and only then moves
/// the company onto the plan (its add-ons follow the plan from now on). Safe
/// to retry: one invite never makes two subscriptions, and an accepted
/// invite answers 200 again. 503 `acceptance_retry`: nothing was charged,
/// call again shortly. 503 `payment_setup_unavailable`: payment methods
/// cannot be added yet (nothing changed). 409 `invite_needs_new_date`: the
/// first charge is less than 8 days away, or the company's billing changed
/// since the invite was sent (ask for a new invite). 409 `legacy_overdue`:
/// the company has an overdue payment or a balance due. 409
/// `invite_subscription_ended`: the invite's subscription was cancelled in
/// Stripe, so the switch stopped and the company stays on its current plan
/// (ask Granite Manager to withdraw the invite and send a new one).
#[crate::openapi::path(
    post,
    path = "/v1/plan-invites/{token}/acceptance",
    operation_id = "accept_plan_invite",
    tag = "billing",
    params(("token" = String, Path, description = "The token of the emailed `/upgrade/<token>` link")),
    request_body = AcceptanceBody,
    responses(
        (status = 200, description = "The company is on the plan", body = AcceptanceDone),
        (status = 400, description = "Malformed body", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a Manager of the invited company (code not_invited)", body = CodedErrorBody),
        (status = 404, description = "Unknown link (code invite_not_found)", body = CodedErrorBody),
        (status = 409, description = "Texts changed, the first charge is under 8 days away or the company's billing changed, the company owes money, or the invite's subscription was cancelled in Stripe (codes consents_changed, invite_needs_new_date, legacy_overdue, invite_subscription_ended)", body = CodedErrorBody),
        (status = 410, description = "The invite expired or was withdrawn (codes invite_expired, invite_revoked)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Try again shortly, or payment methods are not available yet (codes acceptance_retry, payment_setup_unavailable)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn accept_plan_invite(
    State(state): State<AppState>,
    PositionedUser(user, positions): PositionedUser,
    Path(token): Path<String>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<AcceptanceBody>,
) -> ApiResult<Json<AcceptanceDone>> {
    let caller = Caller {
        user: &user,
        positions: &positions,
    };
    let accepted = accept::accept(
        &state,
        &token,
        caller,
        &body.consents,
        &RequestMeta::from_headers(&headers),
    )
    .await?;
    Ok(Json(AcceptanceDone {
        status: "accepted".into(),
        redirect_url: accepted.redirect_url,
    }))
}
