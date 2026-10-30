//! `/v1/me/ringcentral`: the user's own `RingCentral` sign-in (My Account >
//! Connect my RingCentral).
//!
//! `RingCentral` lets a login text only from its own numbers, so the
//! company's connection texts only as the person who connected it; a rep who
//! signs in here texts from the CRM as themselves, and their texts arrive
//! through a webhook of their own login (`ringcentral_auth::access_token_as`).

use super::account::{home_actor, stored};
use crate::app_origin::company_subdomain;
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::oauth_state;
use crate::openapi::ToSchema;
use crate::ringcentral_connect::{self, ConnectError};
use crate::routes::company::queries::company_features;
use crate::routes::users::texting_history;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use common::telephony::ringcentral;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};

/// The `state` kind of a rep's sign-in (the company's is `ringcentral`).
const STATE_KIND: &str = "ringcentral-rep";
/// The redirect URI registered on Granite Manager's `RingCentral` app; the
/// company connect returns there too, told apart by the `state`.
const RETURN_PATH: &str = "/redirects/ringcentral/user-auth";

fn redirect_uri(state: &AppState) -> String {
    format!("{}{RETURN_PATH}", state.app_url.trim_end_matches('/'))
}

/// `POST /v1/me/ringcentral/start`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RingcentralSignIn {
    /// `RingCentral`'s sign-in page.
    pub url: String,
}

/// Start connecting my RingCentral
///
/// `RingCentral`'s sign-in page for the caller. `RingCentral` returns to
/// `/redirects/ringcentral/user-auth` with `code` and `state` (or `error`);
/// post them to `POST /v1/me/ringcentral` within 15 minutes.
#[crate::openapi::path(
    post,
    path = "/v1/me/ringcentral/start",
    operation_id = "start_my_ringcentral",
    tag = "me",
    responses(
        (status = 200, description = "RingCentral's sign-in page", body = RingcentralSignIn),
        (status = 400, description = "The company has no RingCentral connection, or connect is not set up", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn start(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<RingcentralSignIn>> {
    let actor = home_actor(&state.pool, user.id).await?;
    let app = connect_app(&state, actor.company_id).await?;
    let state_token = oauth_state::sign(
        &state.session_secret,
        STATE_KIND,
        actor.company_id,
        actor.user_id,
    );
    Ok(Json(RingcentralSignIn {
        url: app.authorize_url(&redirect_uri(&state), &state_token),
    }))
}

/// The app reps sign in through, once the company itself is connected.
async fn connect_app(
    state: &AppState,
    company_id: i32,
) -> ApiResult<&ringcentral_connect::RingCentralApp> {
    let app = state.ringcentral_app.as_ref().ok_or_else(|| {
        ApiError::BadRequest("RingCentral connect is not set up yet. Ask a superuser.".into())
    })?;
    if !company_features(&state.pool, company_id).await?.ringcentral {
        return Err(ApiError::BadRequest(
            "Your company isn't connected to RingCentral. Ask a manager to connect it on the Company page.".into(),
        ));
    }
    Ok(app)
}

/// Body of `POST /v1/me/ringcentral`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RingcentralReturn {
    /// `state` as `RingCentral` returned it.
    pub state: String,
    /// `code` as `RingCentral` returned it; absent when the user cancelled.
    #[serde(default)]
    pub code: Option<String>,
    /// `error` as `RingCentral` returned it (the user cancelled).
    #[serde(default)]
    pub error: Option<String>,
}

/// `POST /v1/me/ringcentral`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RingcentralConnected {
    /// The company's subdomain, to go back to My Account there; null when it
    /// has none.
    pub company_subdomain: Option<String>,
    /// The number the user texts from now (10 digits); null when their
    /// `RingCentral` user has no number that can text.
    pub ringcentral_phone_number: Option<String>,
}

/// Finish connecting my RingCentral
///
/// Trades the code for the caller's own tokens, after checking they signed
/// in to the company's `RingCentral` account. Their line becomes the
/// extension that signed in and its texting number, their earlier texts
/// follow them, and `RingCentral` pushes their new texts to the CRM (a
/// failed subscription does not undo the connect).
#[crate::openapi::path(
    post,
    path = "/v1/me/ringcentral",
    operation_id = "finish_my_ringcentral",
    tag = "me",
    request_body = RingcentralReturn,
    responses(
        (status = 200, description = "Connected", body = RingcentralConnected),
        (status = 400, description = "Cancelled, expired or foreign state, another RingCentral account, or RingCentral refused the code", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn finish(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<RingcentralReturn>,
) -> ApiResult<Json<RingcentralConnected>> {
    if input.error.as_deref().is_some_and(|error| !error.is_empty()) {
        return Err(ApiError::BadRequest(
            "RingCentral connection was cancelled.".into(),
        ));
    }
    let pool = &state.pool;
    let actor = home_actor(pool, user.id).await?;
    if !oauth_state::verify(
        &state.session_secret,
        &input.state,
        STATE_KIND,
        actor.company_id,
        actor.user_id,
    ) {
        return Err(ApiError::BadRequest(
            "RingCentral connection expired. Please try again.".into(),
        ));
    }
    let Some(code) = input.code.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
        return Err(ApiError::BadRequest(
            "Invalid data returned from RingCentral.".into(),
        ));
    };
    let app = connect_app(&state, actor.company_id).await?;
    let before = stored(pool, actor).await?.line();
    let login = match app
        .connect_rep(
            pool,
            &state.telephony_http,
            actor.company_id,
            actor.user_id,
            code,
            &redirect_uri(&state),
        )
        .await
    {
        Ok(login) => login,
        Err(ConnectError::Database(error)) => return Err(ApiError::Database(error)),
        Err(ConnectError::OtherAccount) => {
            return Err(ApiError::BadRequest(
                "That RingCentral login isn't in your company's RingCentral account. Sign in with your own work login.".into(),
            ));
        }
        Err(error) => {
            tracing::warn!(%error, user_id = actor.user_id, "RingCentral rep connect failed");
            return Err(ApiError::BadRequest(
                "Could not finish the RingCentral connection. Try again.".into(),
            ));
        }
    };
    let phone = login.phone.clone().or_else(|| before.ringcentral_phone.clone());
    sqlx::query(
        "UPDATE users SET ringcentral_extension_id = ?, ringcentral_phone_number = ?
          WHERE id = ? AND company_id = ?",
    )
    .bind(&login.extension_id)
    .bind(&phone)
    .bind(actor.user_id)
    .bind(actor.company_id)
    .execute(pool)
    .await?;
    let after = texting_history::Line {
        ringcentral_extension: Some(login.extension_id.clone()),
        ringcentral_phone: phone.clone(),
        ..before.clone()
    };
    texting_history::follow(&state, actor.company_id, actor.user_id, &before, &after, true).await;
    if let Err(error) = ringcentral::ensure_rep_sms_subscription(
        pool,
        &state.telephony_http,
        actor.company_id,
        &login.extension_id,
    )
    .await
    {
        tracing::error!(?error, user_id = actor.user_id, "a rep's RingCentral SMS subscription failed");
    }
    Ok(Json(RingcentralConnected {
        company_subdomain: company_subdomain(pool, actor.company_id).await?,
        ringcentral_phone_number: phone,
    }))
}

/// Disconnect my RingCentral
///
/// Forgets the caller's own `RingCentral` sign-in; their line stays, but
/// texts from the CRM can't go out as them until they connect again.
#[crate::openapi::path(
    delete,
    path = "/v1/me/ringcentral",
    operation_id = "disconnect_my_ringcentral",
    tag = "me",
    responses(
        (status = 204, description = "Disconnected (or was not connected)"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn disconnect(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<StatusCode> {
    let actor = home_actor(&state.pool, user.id).await?;
    ringcentral_connect::disconnect_rep(
        &state.pool,
        &state.telephony_http,
        actor.company_id,
        actor.user_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
