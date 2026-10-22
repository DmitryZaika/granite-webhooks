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
//!
//! Every extractor answers 402 `payment_required` when the company the request
//! acts in has its access paused for billing, except on the paths in
//! [`open_while_paused`].

pub mod cookie;
pub mod session;

use crate::entitlements::company_entitlements;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use cookie::{SESSION_COOKIE_NAME, cookie_values, decode_session};
use lambda_http::tracing;
use session::{SessionUser, company_billing_paused, effective_user, find_session, user_positions};

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
    let row = find_session(&state.pool, &session_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let own_company_id = row.user.company_id;
    let positions = user_positions(&state.pool, row.user.id).await?;
    let mut user = effective_user(row.user, &positions, session.active_company_id);
    if user.company_id != own_company_id {
        // A super admin acting in another company: that company's plan.
        user.plan = company_entitlements(&state.pool, user.company_id).await?;
    }
    if !open_while_paused(&parts.method, parts.uri.path()) {
        let paused = if user.company_id == own_company_id {
            row.billing_paused
        } else {
            // A super admin acting in another company; errors keep access.
            company_billing_paused(&state.pool, user.company_id)
                .await
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "billing pause check failed");
                    false
                })
        };
        if paused {
            return Err(ApiError::PaymentRequired);
        }
    }
    Ok((user, positions))
}

/// Paths (and everything below them) that work while the company's access
/// is paused for billing: paying, signing in and out, the app shell, the
/// bell, superuser tools and the onboarding call / plan invites.
const OPEN_WHILE_PAUSED: [&str; 7] = [
    "/v1/billing",
    "/v1/auth",
    "/v1/app/shell",
    "/v1/notifications",
    "/v1/superuser",
    "/v1/onboarding-call",
    "/v1/plan-invites",
];

/// The path itself or anything below it, so `/v1/billing` does not also open
/// `/v1/billing-reminders` or `/v1/billingx`.
fn is_path_or_child(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether a request works while its company's access is paused. Reading
/// `/v1/me` works too (the SPA needs who is signed in), but not its children.
pub fn open_while_paused(method: &Method, path: &str) -> bool {
    (method == Method::GET && path == "/v1/me")
        || OPEN_WHILE_PAUSED
            .iter()
            .any(|base| is_path_or_child(path, base))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &str) -> bool {
        open_while_paused(&Method::GET, path)
    }

    #[test]
    fn paused_companies_reach_billing_sign_in_and_notifications_only() {
        for path in [
            "/v1/billing",
            "/v1/billing/gate",
            "/v1/billing/reactivation/finish",
            "/v1/auth/login",
            "/v1/auth/logout",
            "/v1/auth/session",
            "/v1/app/shell",
            "/v1/notifications",
            "/v1/notifications/done",
            "/v1/superuser/companies",
            "/v1/onboarding-call",
            "/v1/plan-invites/abc",
            "/v1/me",
        ] {
            assert!(open(path), "{path} should stay open");
        }
        for path in [
            "/v1/customers",
            "/v1/deals/1",
            "/v1/stones",
            "/v1/me/todos",
            "/",
        ] {
            assert!(!open(path), "{path} should be paused");
        }
    }

    #[test]
    fn allowed_prefixes_match_whole_path_segments_only() {
        for path in [
            "/v1/billing-reminders/process",
            "/v1/billingx",
            "/v1/notifications-export",
            "/v1/authx",
            "/v1/app/shellx",
            "/v1/mex",
            "/v1/me/",
        ] {
            assert!(!open(path), "{path} should be paused");
        }
    }

    #[test]
    fn only_reading_me_stays_open() {
        assert!(open_while_paused(&Method::GET, "/v1/me"));
        assert!(!open_while_paused(&Method::PUT, "/v1/me"));
        assert!(!open_while_paused(&Method::PUT, "/v1/me/account"));
        assert!(open_while_paused(&Method::POST, "/v1/billing/checkout"));
    }
}
