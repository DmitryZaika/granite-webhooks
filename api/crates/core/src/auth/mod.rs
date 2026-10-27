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
//! with `GET` (what the phone's composers, schedule, Text tab and More
//! screens read: calendars, deals and their stages, customers, stones,
//! suppliers, sales, quotes, images, documents, email and text templates, my texts
//! over every provider, a pushed CRM notification), on
//! `/v1/sms/{provider}/messages` with `POST`
//! (sending those texts), on [`DEVICE_ACTIONS`] with `POST` (an
//! attachment upload, Improve message, AI reply, the typing suggestion and
//! what it learns, filling a template) and on
//! the Deals tab's changes (`deal_write`: a deal, its to-dos, notes and
//! files; not deleting a deal or the board setup) the
//! cookie extractors above (`CurrentUser` through `ShopOrEmployeeUser`) also take the
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
pub mod job;
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
/// Use Template lists, the suggestions under an email's To, Cc and Bcc, the
/// schedule's calendars, and the More screens: Deals, Customers, Stones,
/// Suppliers, Images (with its folders), Transactions and Quotes; and the CRM
/// notification a push names).
/// These prefixes and everything under them; writes there still need the CRM
/// session.
pub const DEVICE_READ_ROUTES: [&str; 16] = [
    "/v1/sms/mine",
    "/v1/calendars",
    "/v1/stones",
    "/v1/images",
    "/v1/image-library",
    "/v1/documents",
    "/v1/email-templates",
    "/v1/sms-templates",
    "/v1/customers",
    "/v1/deals",
    "/v1/deal-groups",
    "/v1/deal-lists",
    "/v1/suppliers",
    "/v1/transactions",
    "/v1/quotes",
    "/v1/notifications",
];

/// CRM actions the app's composer calls with its device token: exactly these
/// paths, `POST` only (an attachment's upload URL, a text's files made ready
/// to send, Improve message, AI reply, the suggestion that continues what the
/// rep types and what it learns from the text they sent instead, and a
/// template's `{{variables}}` filled for Use Template).
pub const DEVICE_ACTIONS: [&str; 7] = [
    "/v1/uploads",
    "/v1/sms/attachments",
    "/v1/ai/improve",
    "/v1/ai/conversation-reply",
    "/v1/ai/compose/autocomplete",
    "/v1/ai/compose/learn",
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

/// A whole id segment: digits only.
fn is_id(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
}

/// The changes the app's Deals tab makes with its device token, exactly
/// these: create a deal (`POST /v1/deals`), edit it (`PATCH /v1/deals/{id}`),
/// win, lose or reopen it (`PUT .../outcome`), its to-dos (`POST
/// .../activities`, `PATCH|DELETE .../activities/{id}`), its notes and their
/// comments, its files (`POST .../files`, `DELETE .../files/{source}/{id}`,
/// `PATCH .../documents/{id}`), and the to-do name suggestions
/// (`/v1/deal-activities/suggestions`). Deleting a deal, reordering the
/// board and the pipeline and stage setup stay with the CRM session.
fn deal_write(method: &Method, path: &str) -> bool {
    if let Some(rest) = path.strip_prefix("/v1/deal-activities/suggestions") {
        return (rest.is_empty() && method == Method::GET)
            || (rest == "/hidden" && method == Method::POST);
    }
    let Some(rest) = path.strip_prefix("/v1/deals") else {
        return false;
    };
    if rest.is_empty() {
        return method == Method::POST;
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let (post, put, patch, delete) = (
        method == Method::POST,
        method == Method::PUT,
        method == Method::PATCH,
        method == Method::DELETE,
    );
    match parts.as_slice() {
        [id] => is_id(id) && patch,
        [id, "outcome"] => is_id(id) && put,
        [id, "activities" | "notes" | "files"] => is_id(id) && post,
        [id, "activities" | "notes", item] => is_id(id) && is_id(item) && (patch || delete),
        [id, "notes", note, "comments"] => is_id(id) && is_id(note) && post,
        [id, "notes", note, "comments", comment] => {
            is_id(id) && is_id(note) && is_id(comment) && delete
        }
        [id, "documents", document] => is_id(id) && is_id(document) && patch,
        [id, "files", source, file] => is_id(id) && !source.is_empty() && is_id(file) && delete,
        _ => false,
    }
}

/// The text and email templates the app's Use Template list adds, edits and
/// deletes with its device token: `POST /v1/sms-templates` and
/// `/v1/email-templates`, `PUT|DELETE` on `/{id}` of either (the caller's own,
/// or an admin's company ones, as on the web). Their attachments and the
/// picker order stay with the CRM session.
fn template_write(method: &Method, path: &str) -> bool {
    let rest = path
        .strip_prefix("/v1/sms-templates")
        .or_else(|| path.strip_prefix("/v1/email-templates"));
    match rest {
        Some("") => method == Method::POST,
        Some(rest) => rest
            .strip_prefix('/')
            .is_some_and(|id| is_id(id) && (method == Method::PUT || method == Method::DELETE)),
        None => false,
    }
}

/// A request the device token may sign: any method on [`DEVICE_ROUTES`],
/// `GET` on [`DEVICE_READ_ROUTES`], `POST` on [`DEVICE_ACTIONS`] and on a
/// provider's `/v1/sms/{provider}/messages`, and the Deals tab's changes
/// ([`deal_write`]) and the templates' ([`template_write`]).
fn device_route(method: &Method, path: &str) -> bool {
    DEVICE_ROUTES.iter().any(|prefix| under(path, prefix))
        || (method == Method::GET && DEVICE_READ_ROUTES.iter().any(|prefix| under(path, prefix)))
        || (method == Method::POST && (DEVICE_ACTIONS.contains(&path) || sms_send(path)))
        || deal_write(method, path)
        || template_write(method, path)
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

/// Whether `a` and `b` are equal, in time that depends only on their length.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// SHA-256 of `value` as lowercase hex: how tokens are stored.
pub fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
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
            "/v1/image-library",
            "/v1/documents",
            "/v1/customers/email-recipients",
            "/v1/customers/page",
            "/v1/customers/7",
            "/v1/deal-groups",
            "/v1/deal-lists",
            "/v1/stones",
            "/v1/stones/7",
            "/v1/suppliers/directory",
            "/v1/transactions",
            "/v1/transactions/7",
            "/v1/quotes",
            "/v1/notifications/7",
        ] {
            assert!(device_route(&Method::GET, path), "{path}");
            for method in [Method::POST, Method::PUT, Method::DELETE] {
                assert!(!device_route(&method, path), "{method} {path}");
            }
        }
        for path in ["/v1/calendars/7"] {
            assert!(!device_route(&Method::PUT, path), "{path}");
            assert!(!device_route(&Method::DELETE, path), "{path}");
        }
        for path in [
            "/v1/imagesx",
            "/v1/documentsx",
            "/v1/customersx",
            "/v1/dealsx",
            "/v1/transactionsx/1",
            "/v1/quotesx",
        ] {
            assert!(!device_route(&Method::GET, path), "{path}");
        }
    }

    #[test]
    fn the_deals_tab_changes_deals_but_not_the_board_setup() {
        for (method, path) in [
            (Method::GET, "/v1/deals"),
            (Method::GET, "/v1/deals/7/stage-history"),
            (Method::POST, "/v1/deals"),
            (Method::PATCH, "/v1/deals/7"),
            (Method::PUT, "/v1/deals/7/outcome"),
            (Method::POST, "/v1/deals/7/activities"),
            (Method::PATCH, "/v1/deals/7/activities/3"),
            (Method::DELETE, "/v1/deals/7/activities/3"),
            (Method::POST, "/v1/deals/7/notes"),
            (Method::PATCH, "/v1/deals/7/notes/3"),
            (Method::DELETE, "/v1/deals/7/notes/3"),
            (Method::POST, "/v1/deals/7/notes/3/comments"),
            (Method::DELETE, "/v1/deals/7/notes/3/comments/9"),
            (Method::POST, "/v1/deals/7/files"),
            (Method::DELETE, "/v1/deals/7/files/image/4"),
            (Method::PATCH, "/v1/deals/7/documents/4"),
            (Method::GET, "/v1/deal-activities/suggestions"),
            (Method::POST, "/v1/deal-activities/suggestions/hidden"),
        ] {
            assert!(device_route(&method, path), "{method} {path}");
        }
        for (method, path) in [
            (Method::DELETE, "/v1/deals/7"),
            (Method::PUT, "/v1/deals/7"),
            (Method::POST, "/v1/deals/reorder"),
            (Method::POST, "/v1/deals/7"),
            (Method::PATCH, "/v1/deals/x"),
            (Method::POST, "/v1/deals/7/activities/3"),
            (Method::DELETE, "/v1/deals/7/notes"),
            (Method::POST, "/v1/deals/7/outcome"),
            (Method::PUT, "/v1/deals/7/questionnaire-answers"),
            (Method::POST, "/v1/deal-pipelines"),
            (Method::PATCH, "/v1/deal-pipelines/2"),
            (Method::POST, "/v1/deal-pipelines/2/stages"),
            (Method::DELETE, "/v1/deal-stages/2"),
            (Method::POST, "/v1/dealsx"),
            (Method::DELETE, "/v1/deal-activities/suggestions"),
        ] {
            assert!(!device_route(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn templates_are_added_edited_and_deleted_from_the_phone() {
        for kind in ["sms-templates", "email-templates"] {
            let list = format!("/v1/{kind}");
            let one = format!("/v1/{kind}/3");
            assert!(device_route(&Method::GET, &list), "{list}");
            assert!(device_route(&Method::POST, &list), "{list}");
            assert!(!device_route(&Method::PUT, &list), "{list}");
            assert!(!device_route(&Method::DELETE, &list), "{list}");
            assert!(device_route(&Method::PUT, &one), "{one}");
            assert!(device_route(&Method::DELETE, &one), "{one}");
            assert!(!device_route(&Method::POST, &one), "{one}");
            assert!(!device_route(&Method::PUT, &format!("/v1/{kind}/x")));
        }
        assert!(!device_route(
            &Method::POST,
            "/v1/email-templates/3/attachments"
        ));
        assert!(!device_route(&Method::PUT, "/v1/sms-templates/order"));
        assert!(!device_route(&Method::POST, "/v1/sms-templatesx"));
    }

    #[test]
    fn the_composer_actions_are_exact_posts() {
        for path in [
            "/v1/uploads",
            "/v1/sms/attachments",
            "/v1/ai/improve",
            "/v1/ai/conversation-reply",
            "/v1/ai/compose/autocomplete",
            "/v1/ai/compose/learn",
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
            "/v1/ai/compose",
            "/v1/ai/compose/autocompletex",
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
