//! Request authentication. Handlers take one of the extractors below; the
//! extractor rejects with 401/403 before the handler runs.
//!
//! | Extractor      | Remix equivalent            | Passes when                              |
//! |----------------|-----------------------------|------------------------------------------|
//! | `CurrentUser`  | `requireLoggedInUser`*      | any valid session                        |
//! | `EmployeeUser` | `getEmployeeUser`           | `is_employee \|\| is_admin \|\| is_superuser` |
//! | `AdminUser`    | `getAdminUser`              | `is_admin \|\| is_superuser`              |
//!
//! *`CurrentUser` also applies the super-admin elevation and company switch, so
//! `company_id` is always the company the request acts in.

pub mod cookie;
pub mod session;

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use cookie::{SESSION_COOKIE_NAME, cookie_value, decode_session};
use session::{SessionUser, effective_user, find_session_user, user_positions};

async fn authenticate(parts: &Parts, state: &AppState) -> Result<SessionUser, ApiError> {
    let session = parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find_map(|header| cookie_value(header, SESSION_COOKIE_NAME))
        .and_then(|value| decode_session(&value, &state.session_secret))
        .ok_or(ApiError::Unauthorized)?;
    let session_id = session.session_id.ok_or(ApiError::Unauthorized)?;
    let user = find_session_user(&state.pool, &session_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let positions = user_positions(&state.pool, user.id).await?;
    Ok(effective_user(user, &positions, session.active_company_id))
}

pub struct CurrentUser(pub SessionUser);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        authenticate(parts, state).await.map(Self)
    }
}

pub struct EmployeeUser(pub SessionUser);

impl FromRequestParts<AppState> for EmployeeUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let user = authenticate(parts, state).await?;
        if user.is_employee || user.is_admin || user.is_superuser {
            Ok(Self(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

pub struct AdminUser(pub SessionUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let user = authenticate(parts, state).await?;
        if user.is_admin || user.is_superuser {
            Ok(Self(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}
