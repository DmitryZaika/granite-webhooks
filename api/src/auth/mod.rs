//! Request authentication. Handlers take one of the extractors below; the
//! extractor rejects with 401/403 before the handler runs.
//!
//! | Extractor      | Remix equivalent            | Passes when                              |
//! |----------------|-----------------------------|------------------------------------------|
//! | `CurrentUser`  | `requireLoggedInUser`*      | any valid session                        |
//! | `EmployeeUser` | `getEmployeeUser`           | `is_employee \|\| is_admin \|\| is_superuser` |
//! | `AdminUser`    | `getAdminUser`              | `is_admin \|\| is_superuser`              |
//! | `UserManager`  | `canEditAdminUsers`         | a superuser, or an admin holding Manager in the company |
//! | `InstallerUser` | `getInstallerUser` | an installer of the company |
//! | `ShopOrEmployeeUser` | `getShopOrEmployeeUser` | an employee, or a shop manager / worker of the company |
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
use cookie::{SESSION_COOKIE_NAME, cookie_values, decode_session};
use session::{SessionUser, effective_user, find_session_user, user_positions};

async fn authenticate(parts: &Parts, state: &AppState) -> Result<SessionUser, ApiError> {
    authenticate_with_positions(parts, state)
        .await
        .map(|(user, _)| user)
}

async fn authenticate_with_positions(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>), ApiError> {
    let session = parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| cookie_values(header, SESSION_COOKIE_NAME))
        .find_map(|value| decode_session(&value, &state.session_secret))
        .ok_or(ApiError::Unauthorized)?;
    let session_id = session.session_id.ok_or(ApiError::Unauthorized)?;
    let user = find_session_user(&state.pool, &session_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let positions = user_positions(&state.pool, user.id).await?;
    let user = effective_user(user, &positions, session.active_company_id);
    Ok((user, positions))
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

/// Shop manager and shop worker positions (`Positions` in the CRM).
const SHOP_POSITIONS: [i32; 2] = [3, 4];

/// An employee, or someone of the shop (a shop manager or worker of the
/// company the request acts in).
pub struct ShopOrEmployeeUser(pub SessionUser);

impl FromRequestParts<AppState> for ShopOrEmployeeUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        let shop = positions.iter().any(|row| {
            row.company_id == user.company_id && SHOP_POSITIONS.contains(&row.position_id)
        });
        if user.is_employee || user.is_admin || user.is_superuser || shop {
            Ok(Self(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

/// `positions.id` of an installer.
const INSTALLER_POSITION: i32 = 6;

/// An installer of the company the request acts in (the installer portal).
pub struct InstallerUser(pub SessionUser);

impl FromRequestParts<AppState> for InstallerUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        let installer = positions
            .iter()
            .any(|row| row.company_id == user.company_id && row.position_id == INSTALLER_POSITION);
        if installer {
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

/// `positions.id` of a manager.
const MANAGER_POSITION: i32 = 5;

/// Who may add, edit and delete users (`canEditAdminUsers`): a superuser, or
/// an admin who is a Manager of the company the request acts in.
pub struct UserManager(pub SessionUser);

impl FromRequestParts<AppState> for UserManager {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        let manager = positions
            .iter()
            .any(|row| row.company_id == user.company_id && row.position_id == MANAGER_POSITION);
        if user.is_superuser || ((user.is_admin) && manager) {
            Ok(Self(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

/// Who may pay for the company (`userCanManageBilling`): an employee who is
/// a Manager of the company the request acts in.
pub struct BillingManager(pub SessionUser);

impl FromRequestParts<AppState> for BillingManager {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        let manager = positions
            .iter()
            .any(|row| row.company_id == user.company_id && row.position_id == MANAGER_POSITION);
        if (user.is_employee || user.is_admin || user.is_superuser) && manager {
            Ok(Self(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}
