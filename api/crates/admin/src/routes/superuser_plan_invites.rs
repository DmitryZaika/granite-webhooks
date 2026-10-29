//! Super admin: "Move to plan" for a legacy company.
//!
//! - `GET    /v1/superuser/companies/{id}/plan-move`                        → what the dialog needs
//! - `POST   /v1/superuser/companies/{id}/plan-invites`                     → invite the Managers
//! - `POST   /v1/superuser/companies/{id}/plan-invites/{invite_id}/emails`  → email new links
//! - `DELETE /v1/superuser/companies/{id}/plan-invites/{invite_id}`         → withdraw an invite
//!
//! Superusers only. See `crate::billing::invites`.

use crate::auth::CurrentUser;
use crate::auth::session::SessionUser;
use crate::billing::invites::{
    self, NewInvite, PlanInviteView, PlanMove, check_addons, check_expiry,
};
use crate::error::{ApiError, ApiResult, CodedErrorBody, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_plan_move))
        .routes(routes!(create_plan_invite))
        .routes(routes!(resend_plan_invite))
        .routes(routes!(revoke_plan_invite))
}

const fn superuser(user: &SessionUser) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Get what moving a company onto a plan needs
///
/// For the "Move to plan" dialog: the company's plan, billable users and
/// Managers (the invite goes to each), its legacy Stripe subscription (when
/// it runs, the first charge date is forced to its period end and cannot be
/// picked, and the invite expires by `latest_expiry_on`), why no invite can
/// be made now (`blocked`), the price list, the dialog's defaults and
/// allowed dates (a picked first charge day is at least
/// `min_days_after_expiry` days after the invite expires), and its invites
/// (newest first, at most 20). Then `create_plan_invite`.
#[crate::openapi::path(
    get,
    path = "/v1/superuser/companies/{id}/plan-move",
    operation_id = "get_platform_company_plan_move",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    responses(
        (status = 200, description = "What the dialog needs", body = PlanMove),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_plan_move(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<PlanMove>> {
    superuser(&user)?;
    Ok(Json(invites::plan_move(&state, id).await?))
}

/// Body of `POST /v1/superuser/companies/{id}/plan-invites`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePlanInviteBody {
    /// The plan's add-ons: `signatures`, `quotes`, `quickbooks` (none for the
    /// base plan only).
    #[serde(default)]
    pub addons: Vec<String>,
    /// The first charge day (`YYYY-MM-DD`): within today + 730 days, and at
    /// least 8 days after the day the invite expires. Ignored (may be null)
    /// when the company has a running legacy subscription: its period end is
    /// used.
    #[serde(default)]
    pub first_charge_on: Option<String>,
    /// Days the links work, 1 to 30 (default 14): until 23:59:59
    /// Indianapolis time on that day. With a running legacy subscription the
    /// invite ends no later than the end of the day 8 days before its period
    /// end's day (`latest_expiry_on`).
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

/// Answer of `POST /v1/superuser/companies/{id}/plan-invites`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedPlanInvite {
    pub invite: PlanInviteView,
    /// Managers the link was emailed to.
    pub emailed: usize,
    /// Addresses the email could not be sent to (resend later). Every
    /// address when sending failed altogether: the invite is still made.
    pub not_emailed: Vec<String>,
}

/// Invite a company's Managers to a plan
///
/// Makes an invite with the add-ons and first charge day (forced to the
/// legacy subscription's period end when one runs), replacing the company's
/// open invite, and emails every Manager of the company their own link to
/// review and accept it on the payments app. Nothing changes for the
/// company until a Manager accepts. Once the invite is made the answer is
/// 201, with addresses that could not be emailed in `not_emailed`. 400
/// codes: `invalid_addon`, `invalid_first_charge`, `invalid_expiry`; 409:
/// the `blocked` codes of `get_platform_company_plan_move`
/// (`already_on_plan`, `invite_accepting`, `payment_unavailable`,
/// `no_managers`, `legacy_overdue`, `onboarding_advance_open`,
/// `legacy_subscription_unsupported`, `legacy_period_too_close`); 503: `catalog_unavailable` (nothing made).
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/plan-invites",
    operation_id = "create_platform_company_plan_invite",
    tag = "superuser",
    params(("id" = i32, Path, description = "Company id")),
    request_body = CreatePlanInviteBody,
    responses(
        (status = 201, description = "Made; emailed except `not_emailed`", body = CreatedPlanInvite),
        (status = 400, description = "Bad add-on, date or expiry (codes invalid_addon, invalid_first_charge, invalid_expiry)", body = CodedErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such company", body = ErrorBody),
        (status = 409, description = "The company cannot be invited now (codes already_on_plan, invite_accepting, payment_unavailable, no_managers, legacy_overdue, onboarding_advance_open, legacy_subscription_unsupported, legacy_period_too_close)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Prices cannot be loaded now (code catalog_unavailable), or billing is busy", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_plan_invite(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i32>,
    JsonBody(body): JsonBody<CreatePlanInviteBody>,
) -> ApiResult<(StatusCode, Json<CreatedPlanInvite>)> {
    superuser(&user)?;
    let request = NewInvite {
        addons: check_addons(&body.addons)?,
        first_charge_on: body.first_charge_on,
        expires_in_days: check_expiry(body.expires_in_days)?,
    };
    let (invite, sent) = invites::create(&state, id, user.id, &request).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedPlanInvite {
            invite: invites::view(&state.pool, &invite).await,
            emailed: sent.emailed,
            not_emailed: sent.not_emailed,
        }),
    ))
}

/// Answer of `POST .../plan-invites/{invite_id}/emails`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ResentPlanInvite {
    /// Managers a new link was emailed to.
    pub emailed: usize,
    /// Addresses the email could not be sent to.
    pub not_emailed: Vec<String>,
}

/// Email a plan invite again
///
/// Emails every current Manager of the company a new link to the open
/// invite (links sent before keep working); addresses that could not be
/// emailed are in `not_emailed`. 409 `invite_closed` when the invite is no
/// longer open (accepting, accepted, withdrawn or expired); 503
/// `catalog_unavailable` before any new link is made.
#[crate::openapi::path(
    post,
    path = "/v1/superuser/companies/{id}/plan-invites/{invite_id}/emails",
    operation_id = "resend_platform_company_plan_invite",
    tag = "superuser",
    params(
        ("id" = i32, Path, description = "Company id"),
        ("invite_id" = String, Path, description = "Invite id from `get_platform_company_plan_move`"),
    ),
    responses(
        (status = 200, description = "Emailed", body = ResentPlanInvite),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such invite in this company", body = ErrorBody),
        (status = 409, description = "The invite is not open (code invite_closed)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Prices cannot be loaded now (code catalog_unavailable)", body = CodedErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn resend_plan_invite(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, invite_id)): Path<(i32, String)>,
) -> ApiResult<Json<ResentPlanInvite>> {
    superuser(&user)?;
    let sent = invites::resend(&state, id, &invite_id).await?;
    Ok(Json(ResentPlanInvite {
        emailed: sent.emailed,
        not_emailed: sent.not_emailed,
    }))
}

/// Withdraw a plan invite
///
/// Its links stop working at once (410 `invite_revoked`). An open invite
/// (expired ones too), or an acceptance that stopped being retried
/// (`gave_up`) while Stripe has no subscription for it that has not ended
/// (a cancelled one does not count). 409 `invite_accepting` while a
/// Manager's acceptance is being finished, `invite_has_subscription` when
/// Stripe still has an active subscription for a given-up acceptance
/// (cancel it in Stripe, then withdraw again),
/// `invite_closed` once accepted or withdrawn. 503 when Stripe cannot be
/// read (nothing changed).
#[crate::openapi::path(
    delete,
    path = "/v1/superuser/companies/{id}/plan-invites/{invite_id}",
    operation_id = "revoke_platform_company_plan_invite",
    tag = "superuser",
    params(
        ("id" = i32, Path, description = "Company id"),
        ("invite_id" = String, Path, description = "Invite id from `get_platform_company_plan_move`"),
    ),
    responses(
        (status = 204, description = "Withdrawn"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not a superuser", body = ErrorBody),
        (status = 404, description = "No such invite in this company", body = ErrorBody),
        (status = 409, description = "Cannot be withdrawn (codes invite_accepting, invite_has_subscription, invite_closed)", body = CodedErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 503, description = "Stripe cannot be read now, or billing is busy; nothing changed", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn revoke_plan_invite(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, invite_id)): Path<(i32, String)>,
) -> ApiResult<StatusCode> {
    superuser(&user)?;
    invites::revoke(&state, id, &invite_id, user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
