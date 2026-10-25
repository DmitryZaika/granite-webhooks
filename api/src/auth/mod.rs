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
//! | `InstallerUser` | `getInstallerUser` | an installer of the company |
//! | `ShopOrEmployeeUser` | `getShopOrEmployeeUser` | an employee, or a shop manager / worker of the company |
//! | `device::DeviceUser` | (Granite Phone app)  | a live `X-Granite-Device` token of an employee, admin or superuser |
//! | `device::PhoneCaller` | (app or CRM)        | the device token when sent, else `EmployeeUser`'s cookie rules |
//!
//! *`CurrentUser` also applies the super-admin elevation and company switch, so
//! `company_id` is always the company the request acts in.
//!
//! On [`DEVICE_ROUTES`] (the app's Emails and Schedule screens, and the
//! installers' and template makers' own schedule), on [`DEVICE_READ_ROUTES`]
//! with `GET` (what the phone's email composer and schedule list: calendars,
//! stones, images, documents, email and text templates, my texts over every
//! provider), on `/v1/sms/{provider}/messages` with `POST` (sending them) and on [`DEVICE_ACTIONS`] with `POST`
//! (an attachment upload, Improve message, AI reply, filling a template) the cookie
//! extractors above (`CurrentUser` through `ShopOrEmployeeUser`) also take the
//! app's `X-Granite-Device` token in place of the cookie: the phone's user, in
//! the device session's company, with the same rights as their CRM session.
//! There the header decides alone (a bad token is 401 even with a cookie); on
//! every other route they ignore it. `device::DeviceUser` and
//! `device::PhoneCaller` take the token on any route, as before.
//! Without a session cookie, every extractor also takes an agent token
//! (`Authorization: Bearer agt_...`): the agent then acts as the person who
//! asked it, read-only, in one company (see [`agent`]).

pub mod agent;
pub mod cookie;
pub mod device;
pub mod session;

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use cookie::{SESSION_COOKIE_NAME, cookie_values, decode_session};
use session::{SessionUser, effective_user, find_session_user, user_positions};

async fn authenticate(parts: &Parts, state: &AppState) -> Result<SessionUser, ApiError> {
    authenticate_with_positions(parts, state)
        .await
        .map(|(user, _)| user)
}

/// The CRM routes the Granite Phone app also calls with its device token.
///
/// These prefixes and everything under them. `/v1/me/schedule` is the
/// schedule-only app of installers and template makers (their web portal).
pub const DEVICE_ROUTES: [&str; 4] = [
    "/v1/emails",
    "/v1/schedule",
    "/v1/events",
    "/v1/me/schedule",
];

/// CRM routes the app reads with its device token, `GET` only (my texts
/// over every provider for the Text tab of a company without Telnyx, the
/// email and text composers' From Stones, From Images, From Documents and
/// Use Template lists, and the schedule's calendars). These prefixes and everything under
/// them.
pub const DEVICE_READ_ROUTES: [&str; 7] = [
    "/v1/sms/mine",
    "/v1/calendars",
    "/v1/stones/attachable",
    "/v1/images",
    "/v1/documents",
    "/v1/email-templates",
    "/v1/sms-templates",
];

/// CRM actions the app's composer calls with its device token: exactly these
/// paths, `POST` only (an attachment's upload URL, a text's files made ready
/// to send, Improve message, AI reply, and a template's `{{variables}}`
/// filled for Use Template).
pub const DEVICE_ACTIONS: [&str; 5] = [
    "/v1/uploads",
    "/v1/sms/attachments",
    "/v1/ai/improve",
    "/v1/ai/conversation-reply",
    "/v1/templates/fill",
];

/// `path` is `prefix` or under it (`/v1/emailsx` is not under `/v1/emails`).
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Providers whose texts the app sends through the CRM's own send route.
const SMS_SEND_PROVIDERS: [&str; 3] = ["cloudtalk", "ringcentral", "telnyx"];

/// `POST /v1/sms/{provider}/messages`: a text sent from the app's Text tab
/// through the provider the conversation answers through.
fn sms_send(path: &str) -> bool {
    path.strip_prefix("/v1/sms/")
        .and_then(|rest| rest.strip_suffix("/messages"))
        .is_some_and(|provider| SMS_SEND_PROVIDERS.contains(&provider))
}

/// A request the device token may sign: any method on [`DEVICE_ROUTES`],
/// `GET` on [`DEVICE_READ_ROUTES`], `POST` on [`DEVICE_ACTIONS`] and on a
/// provider's `/v1/sms/{provider}/messages`.
fn device_route(method: &Method, path: &str) -> bool {
    DEVICE_ROUTES.iter().any(|prefix| under(path, prefix))
        || (method == Method::GET && DEVICE_READ_ROUTES.iter().any(|prefix| under(path, prefix)))
        || (method == Method::POST && (DEVICE_ACTIONS.contains(&path) || sms_send(path)))
}

async fn authenticate_with_positions(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>), ApiError> {
    if device_route(&parts.method, parts.uri.path())
        && let Some(token) = device::device_token(parts)
    {
        let device = device::authenticate_device(&token, state).await?;
        let positions = user_positions(&state.pool, device.user.id).await?;
        return Ok((device.user, positions));
    }
    let session = parts
        .headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| cookie_values(header, SESSION_COOKIE_NAME))
        .find_map(|value| decode_session(&value, &state.session_secret));
    let session = match (session, agent::bearer_token(&parts.headers)) {
        (Some(session), _) => session,
        (None, Some(token)) => return agent::authenticate(parts, state, token).await,
        (None, None) => return Err(ApiError::Unauthorized),
    };
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

/// Someone holding the Manager position in the company the request acts in
/// (the app shell's `can_manage_billing`), whatever their other rights: a
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
    use super::device_route;
    use axum::http::Method;

    #[test]
    fn device_routes_are_whole_prefixes() {
        for path in [
            "/v1/emails",
            "/v1/emails/threads",
            "/v1/schedule/events",
            "/v1/events/7/details",
            "/v1/me/schedule",
        ] {
            assert!(device_route(&Method::GET, path), "{path}");
            assert!(device_route(&Method::POST, path), "{path}");
        }
        for path in [
            "/v1/emailsx",
            "/v1/eventsfoo/1",
            "/v1/customers",
            "/v1/me/devices",
            "/v1/me",
            "/v1/me/schedulex",
            "/v1/me/todos",
            "/emails",
            "",
        ] {
            assert!(!device_route(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn the_composer_lists_are_read_only() {
        for path in [
            "/v1/calendars",
            "/v1/stones/attachable",
            "/v1/images",
            "/v1/documents",
            "/v1/email-templates",
            "/v1/sms-templates",
        ] {
            assert!(device_route(&Method::GET, path), "{path}");
            for method in [Method::POST, Method::PUT, Method::DELETE] {
                assert!(!device_route(&method, path), "{method} {path}");
            }
        }
        for path in ["/v1/calendars/7", "/v1/email-templates/3", "/v1/sms-templates/3"] {
            assert!(!device_route(&Method::PUT, path), "{path}");
            assert!(!device_route(&Method::DELETE, path), "{path}");
        }
        for path in ["/v1/stones", "/v1/stones/7", "/v1/imagesx", "/v1/documentsx"] {
            assert!(!device_route(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn the_composer_actions_are_exact_posts() {
        for path in [
            "/v1/uploads",
            "/v1/sms/attachments",
            "/v1/ai/improve",
            "/v1/ai/conversation-reply",
            "/v1/templates/fill",
        ] {
            assert!(device_route(&Method::POST, path), "{path}");
            assert!(!device_route(&Method::GET, path), "{path}");
        }
        for path in [
            "/v1/uploads/x",
            "/v1/ai",
            "/v1/ai/email-template",
            "/v1/ai/agentic-questions",
            "/v1/templates",
            "/v1/sms/other/messages",
            "/v1/sms/ringcentral/messages/1",
            "/v1/sms/ringcentral/threads/5551234567/read",
        ] {
            assert!(!device_route(&Method::POST, path), "{path}");
        }
    }

    #[test]
    fn my_texts_read_and_send_through_the_crm() {
        for path in [
            "/v1/sms/mine/threads",
            "/v1/sms/mine/threads/3175551234",
            "/v1/sms/mine/unread-count",
        ] {
            assert!(device_route(&Method::GET, path), "{path}");
            assert!(!device_route(&Method::POST, path), "{path}");
        }
        for provider in ["cloudtalk", "ringcentral", "telnyx"] {
            let path = format!("/v1/sms/{provider}/messages");
            assert!(device_route(&Method::POST, &path), "{path}");
            assert!(!device_route(&Method::GET, &path), "{path}");
        }
        // The whole company's inbox stays with the CRM session.
        assert!(!device_route(&Method::GET, "/v1/sms/all/threads"));
        assert!(!device_route(&Method::GET, "/v1/sms/minex/threads"));
    }
}
