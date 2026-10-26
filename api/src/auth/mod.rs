//! Request authentication. Handlers take one of the extractors below; the
//! extractor rejects with 401/403 before the handler runs.
//!
//! | Extractor      | Remix equivalent            | Passes when                              |
//! |----------------|-----------------------------|------------------------------------------|
//! | `CurrentUser`  | `requireLoggedInUser`*      | any valid session                        |
//! | `EmployeeUser` | `getEmployeeUser`           | `is_employee \|\| is_admin \|\| is_superuser` |
//! | `AdminUser`    | `getAdminUser`              | `is_admin \|\| is_superuser`              |
//! | `UserManager`  | `canEditAdminUsers`         | a superuser, or an admin holding Manager in the company |
//! | `CompanyManager` | (`can_manage_billing`)    | anyone holding Manager in the company (superusers too only then) |
//! | `PositionedUser` | -                         | any valid session; also gives the user's positions in every company |
//! | `InstallerUser` | `getInstallerUser` | an installer of the company |
//! | `ShopOrEmployeeUser` | `getShopOrEmployeeUser` | an employee, or a shop manager / worker of the company |
//! | `device::DeviceUser` | (Granite Phone app)  | a live `X-Granite-Device` token of an employee, admin or superuser |
//! | `device::PhoneCaller` | (app or CRM)        | the device token when sent, else `EmployeeUser`'s cookie rules |
//!
//! *`CurrentUser` also applies the super-admin elevation and company switch, so
//! `company_id` is always the company the request acts in.
//!
//! Every extractor answers 402 `payment_required` when the company the request
//! acts in has its access paused for billing, except on the paths in
//! [`open_while_paused`].
//!
//! On [`DEVICE_ROUTES`] (the app's Emails and Schedule screens) the cookie
//! extractors above (`CurrentUser` through `ShopOrEmployeeUser`) also take the
//! app's `X-Granite-Device` token in place of the cookie: the phone's user, in
//! the device session's company, with the same rights as their CRM session.
//! There the header decides alone (a bad token is 401 even with a cookie); on
//! every other route they ignore it. `device::DeviceUser` and
//! `device::PhoneCaller` take the token on any route, as before.

pub mod cookie;
pub mod device;
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

/// The CRM routes the Granite Phone app also calls with its device token:
/// these prefixes and everything under them.
pub const DEVICE_ROUTES: [&str; 3] = ["/v1/emails", "/v1/schedule", "/v1/events"];

/// `path` is one of [`DEVICE_ROUTES`] or under one (`/v1/emailsx` is not).
fn device_route(path: &str) -> bool {
    DEVICE_ROUTES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

async fn authenticate_with_positions(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>), ApiError> {
    if device_route(parts.uri.path())
        && let Some(token) = device::device_token(parts)
    {
        let device = device::authenticate_device(&token, state).await?;
        if company_paused(state, device.user.company_id).await {
            return Err(ApiError::PaymentRequired);
        }
        let positions = user_positions(&state.pool, device.user.id).await?;
        return Ok((device.user, positions));
    }
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
            // A super admin acting in another company.
            company_paused(state, user.company_id).await
        };
        if paused {
            return Err(ApiError::PaymentRequired);
        }
    }
    Ok((user, positions))
}

/// Whether `company_id`'s access is paused for billing; errors keep access.
async fn company_paused(state: &AppState, company_id: i32) -> bool {
    company_billing_paused(&state.pool, company_id)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "billing pause check failed");
            false
        })
}

/// Paths (and everything below them) that work while the company's access
/// is paused for billing: paying, signing in and out, the app shell, the
/// bell, superuser tools, the onboarding call / plan invites, and the phone
/// (calls, texts and 911 must keep working; homeowners still call in).
const OPEN_WHILE_PAUSED: [&str; 8] = [
    "/v1/billing",
    "/v1/auth",
    "/v1/app/shell",
    "/v1/notifications",
    "/v1/superuser",
    "/v1/onboarding-call",
    "/v1/plan-invites",
    "/v1/phone",
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

/// Any signed-in user, with their positions in every company: for routes
/// that check a position in a company other than the one the request acts
/// in (a plan invite's company).
pub struct PositionedUser(pub SessionUser, pub Vec<session::UserPositionRow>);

impl FromRequestParts<AppState> for PositionedUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        Ok(Self(user, positions))
    }
}

/// Someone holding the Manager position in the company the request acts in.
///
/// The app shell's `can_manage_billing`, whatever their other rights: a
/// company admin or a superuser without it is refused. Used where only the
/// company's Managers may act (the 10DLC texting registration).
pub struct CompanyManager(pub SessionUser);

impl FromRequestParts<AppState> for CompanyManager {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (user, positions) = authenticate_with_positions(parts, state).await?;
        let manager = positions
            .iter()
            .any(|row| row.company_id == user.company_id && row.position_id == MANAGER_POSITION);
        if manager {
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
            "/v1/phone/calls",
            "/v1/phone/texts/index",
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
            "/v1/phonex",
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

    #[test]
    fn device_routes_are_whole_prefixes() {
        for path in [
            "/v1/emails",
            "/v1/emails/threads",
            "/v1/schedule/events",
            "/v1/events/7/details",
        ] {
            assert!(device_route(path), "{path}");
        }
        for path in [
            "/v1/emailsx",
            "/v1/eventsfoo/1",
            "/v1/customers",
            "/v1/me/devices",
            "/emails",
            "",
        ] {
            assert!(!device_route(path), "{path}");
        }
    }
}
