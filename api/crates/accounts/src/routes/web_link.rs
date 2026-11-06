//! The Granite Phone app opens CRM pages already signed in.
//!
//! - `POST /v1/auth/web-link`         → a link to a CRM page (device token)
//! - `GET  /v1/auth/web-link/{token}` → follow it: the session cookie, then the page
//!
//! The app opens the quote editor (and any other CRM page it has no screen
//! for) in the phone's browser through these, so the browser does not ask
//! for the password the app was signed in with. A link names its page and
//! works for two minutes, while the device session that asked for it is
//! signed in (`auth::web_link`). Following it reuses the browser's own
//! session when it already signs the same user in; otherwise it starts a
//! session as `POST /v1/auth/login` does.

use crate::app_origin::{app_origin, company_subdomain};
use crate::auth::cookie::{SessionData, encode_session, request_session_id};
use crate::auth::device::DeviceUser;
use crate::auth::session::{
    effective_user, find_session_user, find_user, keep_session, user_positions,
};
use crate::auth::web_link::{self, Claims, TOKEN_TTL_SECS};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, LOCATION, REFERRER_POLICY, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_web_link))
        .routes(routes!(open_web_link))
}

/// Body of `POST /v1/auth/web-link`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WebLinkInput {
    /// The CRM page to open: a path on the company's CRM host, starting with
    /// one `/`, e.g. `/employee/quote/12` (a quote's editor) or
    /// `/employee/quotes/add` (a new quote). At most 512 characters.
    pub path: String,
}

/// A link that opens a CRM page signed in.
#[derive(Debug, Serialize, ToSchema)]
pub struct WebLink {
    /// The link's path on this API (`/v1/auth/web-link/{token}`): open the
    /// API's base URL followed by it in the phone's browser.
    pub link_path: String,
    /// Seconds the link works for (120).
    pub expires_in: i64,
}

/// Get a link that opens a CRM page signed in
///
/// For the Granite Phone app: returns a link to `path` on the caller's CRM
/// host that signs the phone's browser in as the caller on the way. Open it
/// at once (the API's base URL + `link_path`): it works for `expires_in`
/// seconds, and only while this phone stays signed in. Device token only.
#[crate::openapi::path(
    post,
    path = "/v1/auth/web-link",
    operation_id = "create_web_link",
    tag = "phone",
    request_body = WebLinkInput,
    responses(
        (status = 200, description = "The link", body = WebLink),
        (status = 400, description = "The path is not a page of the CRM's host", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn create_web_link(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<WebLinkInput>,
) -> ApiResult<Response> {
    let path = web_link::page_path(input.path.trim())
        .ok_or_else(|| ApiError::BadRequest("Not a page of the CRM".into()))?;
    let token = web_link::issue(
        &state.session_secret,
        &Claims {
            s: device.session_id,
            u: device.user.id,
            p: path.to_string(),
            exp: chrono::Utc::now().timestamp() + TOKEN_TTL_SECS,
        },
    );
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(WebLink {
            link_path: format!("/v1/auth/web-link/{token}"),
            expires_in: TOKEN_TTL_SECS,
        }),
    )
        .into_response())
}

/// Open a CRM page signed in
///
/// Follows a link from `POST /v1/auth/web-link`: redirects (303) to its page
/// on the company's CRM host, with a session cookie for the link's user. A
/// browser already signed in as that user keeps its session; otherwise a new
/// one starts, as signing in with the password does. An expired or changed
/// link, or one whose phone was signed out since, redirects to the CRM's
/// sign-in page instead. Takes no other credentials.
#[crate::openapi::path(
    get,
    path = "/v1/auth/web-link/{token}",
    operation_id = "open_web_link",
    tag = "phone",
    params(("token" = String, Path, description = "The token of the link")),
    responses(
        (status = 303, description = "To the page signed in, or to the sign-in page when the link no longer works"),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn open_web_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let now = chrono::Utc::now().timestamp();
    let Some(claims) = web_link::verify(&state.session_secret, &token, now) else {
        return Ok(redirect(&format!("{}/login", state.app_url), Vec::new()));
    };
    // The phone that asked is still signed in, as this user.
    let company: Option<i32> = sqlx::query_scalar(
        "SELECT ds.company_id FROM device_sessions ds
           JOIN company co ON co.id = ds.company_id AND co.deleted_at IS NULL
          WHERE ds.id = ? AND ds.user_id = ? AND ds.revoked_at IS NULL",
    )
    .bind(claims.s)
    .bind(claims.u)
    .fetch_optional(&state.pool)
    .await?;
    let user = match company {
        Some(company) => find_user(&state.pool, claims.u, company).await?,
        None => None,
    };
    let Some(user) = user else {
        return Ok(redirect(&format!("{}/login", state.app_url), Vec::new()));
    };
    let positions = user_positions(&state.pool, user.id).await?;
    let user = effective_user(user, &positions, None);
    if !(user.is_employee || user.is_admin || user.is_superuser) {
        return Ok(redirect(&format!("{}/login", state.app_url), Vec::new()));
    }
    let subdomain = company_subdomain(&state.pool, user.company_id).await?;
    let page = format!(
        "{}{}",
        app_origin(&state.app_url, subdomain.as_deref()),
        claims.p
    );

    // A browser already signed in as this user keeps its session.
    if let Some(id) = request_session_id(&headers, &state.session_secret)
        && let Some(signed_in) = find_session_user(&state.pool, &id).await?
        && signed_in.id == user.id
    {
        keep_session(&state.pool, &id).await?;
        return Ok(redirect(&page, Vec::new()));
    }
    let session_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO sessions (id, user_id, expiration_date) VALUES (?, ?, NULL)")
        .bind(&session_id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    let cookie = encode_session(
        &SessionData {
            session_id: Some(session_id),
            active_company_id: None,
        },
        &state.session_secret,
    );
    Ok(redirect(&page, state.cookie.set_session(&cookie)))
}

/// A 303 to `location` that sets `cookies`, kept out of caches and with no
/// referrer (the link's token stays out of the page's requests).
fn redirect(location: &str, cookies: Vec<String>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(LOCATION, value);
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            headers.append(SET_COOKIE, value);
        }
    }
    response
}
