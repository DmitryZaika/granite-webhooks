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
//! [`open_while_paused`]. `device::DeviceUser` and `device::PhoneCaller` are
//! refused only on the phone admin setup ([`phone_admin_paused`]).
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
//!
//! Every extractor above also applies the position sections ([`sections`]):
//! someone holding only Inventory Manager or Scheduling Manager is refused
//! outside the sections their company gives those positions. Inside them
//! they pass `EmployeeUser` and `ShopOrEmployeeUser`, and `AdminUser` on an
//! admin section's routes.

pub mod agent;
pub mod cookie;
pub mod device;
pub mod job;
pub mod sections;
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
use sections::Access;
use session::{SessionUser, company_billing_paused, effective_user, find_session, user_positions};

async fn authenticate(parts: &Parts, state: &AppState) -> Result<SessionUser, ApiError> {
    authenticate_with_access(parts, state)
        .await
        .map(|(user, _, _)| user)
}

async fn authenticate_with_positions(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>), ApiError> {
    authenticate_with_access(parts, state)
        .await
        .map(|(user, positions, _)| (user, positions))
}

/// Who is asking and what their positions open. A request outside the
/// sections of a sectioned position is refused here, before any handler.
async fn authenticate_with_access(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>, Access), ApiError> {
    let (user, positions) = identify(parts, state).await?;
    let access = sections::user_access(&state.pool, &user, &positions).await?;
    if !access.allows(parts.uri.path()) {
        return Err(sections::outside_sections());
    }
    Ok((user, positions, access))
}

/// [`authenticate_with_access`]'s section check for the phone app's device
/// token (`device::DeviceUser`, `device::PhoneCaller`).
async fn check_sections(
    parts: &Parts,
    state: &AppState,
    user: &SessionUser,
) -> Result<(), ApiError> {
    let positions = user_positions(&state.pool, user.id).await?;
    if sections::user_access(&state.pool, user, &positions)
        .await?
        .allows(parts.uri.path())
    {
        Ok(())
    } else {
        Err(sections::outside_sections())
    }
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

async fn identify(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>), ApiError> {
    if device_route(&parts.method, parts.uri.path())
        && let Some(token) = device::device_token(parts)
    {
        let device = device::authenticate_device(&token, state).await?;
        if !open_while_paused(&parts.method, parts.uri.path())
            && company_paused(state, device.user.company_id).await
        {
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
        .find_map(|value| decode_session(&value, &state.session_secret));
    let session = match (session, agent::bearer_token(&parts.headers)) {
        (Some(session), _) => session,
        (None, Some(token)) => {
            let (mut user, positions) = agent::authenticate(parts, state, token).await?;
            // The agent acts in the question's company: that company's plan,
            // and its billing pause as for a session.
            user.plan = company_entitlements(&state.pool, user.company_id).await?;
            if !open_while_paused(&parts.method, parts.uri.path())
                && company_paused(state, user.company_id).await
            {
                return Err(ApiError::PaymentRequired);
            }
            return Ok((user, positions));
        }
        (None, None) => return Err(ApiError::Unauthorized),
    };
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
/// (calls, texts and 911 must keep working; homeowners still call in) but
/// for its paid admin setup ([`phone_admin_paused`]).
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

/// The phone's admin setup, refused while paused (402): numbers (search,
/// buy, import, edit), porting, 10DLC, connecting Telnyx, hours and call
/// recording. The admins' device list and revoke stay open (a lost phone).
const PHONE_ADMIN: &str = "/v1/phone/admin";
const PHONE_ADMIN_OPEN: &str = "/v1/phone/admin/devices";

/// A phone admin setup path, refused while the company is paused.
pub fn phone_admin_paused(path: &str) -> bool {
    is_path_or_child(path, PHONE_ADMIN) && !is_path_or_child(path, PHONE_ADMIN_OPEN)
}

/// Whether a request works while its company's access is paused. Reading
/// `/v1/me` works too (the SPA needs who is signed in), but not its children.
pub fn open_while_paused(method: &Method, path: &str) -> bool {
    (method == Method::GET && path == "/v1/me")
        || (OPEN_WHILE_PAUSED
            .iter()
            .any(|base| is_path_or_child(path, base))
            && !phone_admin_paused(path))
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
        let (user, _, access) = authenticate_with_access(parts, state).await?;
        if user.is_employee || user.is_admin || user.is_superuser || access != Access::Full {
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
        let (user, positions, access) = authenticate_with_access(parts, state).await?;
        let shop = positions.iter().any(|row| {
            row.company_id == user.company_id && SHOP_POSITIONS.contains(&row.position_id)
        });
        let sectioned = access != Access::Full;
        if user.is_employee || user.is_admin || user.is_superuser || shop || sectioned {
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
        let (user, _, access) = authenticate_with_access(parts, state).await?;
        if user.is_admin || user.is_superuser || access.allows_admin(parts.uri.path()) {
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

/// The origin custom header CloudFront adds to every request it forwards.
///
/// Set by `infra/api-domain.yaml` (parameter `OriginVerifySecret`). A viewer
/// cannot set it: CloudFront replaces a viewer header of that name.
pub const ORIGIN_VERIFY_HEADER: &str = "x-granite-origin-verify";

/// `CLOUDFRONT_ORIGIN_SECRET`, the value CloudFront sends in
/// [`ORIGIN_VERIFY_HEADER`]; read once. Unset: every request is trusted to
/// come through CloudFront (the behaviour before the header existed).
pub fn origin_secret() -> Option<&'static str> {
    static SECRET: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| {
        let secret = std::env::var("CLOUDFRONT_ORIGIN_SECRET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if secret.is_none() {
            tracing::warn!("{}", crate::env_check::ORIGIN_SECRET_WARNING);
        }
        secret
    });
    SECRET.as_deref()
}

/// Whether the request carries CloudFront's origin secret. Compared as
/// SHA-256 digests in constant time, so neither the value nor its length
/// leaks.
fn from_cloudfront(headers: &axum::http::HeaderMap, secret: &str) -> bool {
    use sha2::{Digest, Sha256};
    headers.get(ORIGIN_VERIFY_HEADER).is_some_and(|value| {
        constant_time_eq(
            &Sha256::digest(value.as_bytes()),
            &Sha256::digest(secret.as_bytes()),
        )
    })
}

/// The client address, for consent records, the login throttle and the
/// per-IP signup limit.
///
/// The api runs behind CloudFront in front of its Lambda function URL, but
/// the function URL is public too. With `origin_secret` set
/// ([`origin_secret`]), `X-Forwarded-For` is read only when the request
/// carries that secret in [`ORIGIN_VERIFY_HEADER`]; any other request is a
/// direct call and its client is `source_ip` (the request context's
/// `sourceIp`), else "unknown". Without the secret every request is read as
/// coming through CloudFront (rollout compatibility; `env_check` warns).
///
/// Through CloudFront: CloudFront appends the viewer's address to
/// `X-Forwarded-For`, and the function URL may append its own caller,
/// CloudFront (`source_ip`). Entries the client sent come first and are
/// never trusted: the address is the right-most entry once trailing copies of
/// `source_ip` are dropped, else `source_ip`, else "unknown".
pub fn client_ip_from(
    source_ip: Option<&str>,
    headers: &axum::http::HeaderMap,
    origin_secret: Option<&str>,
) -> String {
    let source = source_ip.map(str::trim).filter(|ip| !ip.is_empty());
    if origin_secret.is_some_and(|secret| !from_cloudfront(headers, secret)) {
        return source.unwrap_or("unknown").to_string();
    }
    let forwarded: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    let mut hops = forwarded.as_slice();
    while let Some((last, rest)) = hops.split_last()
        && Some(*last) == source
    {
        hops = rest;
    }
    hops.last()
        .copied()
        .or(source)
        .unwrap_or("unknown")
        .to_string()
}

/// The `sourceIp` of the Lambda request context, when there is one.
pub fn source_ip(extensions: &axum::http::Extensions) -> Option<String> {
    match extensions.get::<lambda_http::request::RequestContext>()? {
        lambda_http::request::RequestContext::ApiGatewayV2(context) => {
            context.http.source_ip.clone()
        }
        _ => None,
    }
}

/// [`client_ip_from`] without the request context.
///
/// The right-most `X-Forwarded-For` entry of a request that came through
/// CloudFront. Prefer [`ClientIp`], which also drops the function URL's own
/// hop and knows a direct call's address.
pub fn client_ip(headers: &axum::http::HeaderMap) -> String {
    client_ip_from(None, headers, origin_secret())
}

/// [`client_ip_from`] of a request: the client address, as a handler
/// argument.
pub struct ClientIp(pub String);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    #[allow(clippy::unused_async_trait_impl)] // the trait's signature
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(client_ip_from(
            source_ip(&parts.extensions).as_deref(),
            &parts.headers,
            origin_secret(),
        )))
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
            "/v1/phone/e911",
            "/v1/phone/devices",
            "/v1/phone/admin/devices",
            "/v1/phone/admin/devices/7",
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
    fn the_phones_paid_admin_setup_is_paused() {
        for path in [
            "/v1/phone/admin",
            "/v1/phone/admin/numbers",
            "/v1/phone/admin/numbers/buy",
            "/v1/phone/admin/numbers/import",
            "/v1/phone/admin/ports/7/submit",
            "/v1/phone/admin/tendlc",
            "/v1/phone/admin/tendlc/ready",
            "/v1/phone/admin/hours",
            "/v1/phone/admin/recording",
            "/v1/phone/admin/connect",
        ] {
            assert!(phone_admin_paused(path), "{path}");
            assert!(
                !open_while_paused(&Method::POST, path),
                "{path} should be paused"
            );
        }
        assert!(!phone_admin_paused("/v1/phone/adminx"));
        assert!(phone_admin_paused("/v1/phone/admin/devicesx"));
    }

    fn forwarded(forwarded: &str, origin: Option<&str>) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", forwarded.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(ORIGIN_VERIFY_HEADER, origin.parse().unwrap());
        }
        headers
    }

    #[test]
    fn the_client_address_is_the_hop_cloudfront_added() {
        let headers = |value: &str| forwarded(value, None);
        let cloudfront = Some("130.176.1.1");
        // No origin secret configured: every request is read as CloudFront's.
        // The client's own entry is skipped; the function URL's hop dropped.
        assert_eq!(
            client_ip_from(
                cloudfront,
                &headers("6.6.6.6, 203.0.113.9, 130.176.1.1"),
                None
            ),
            "203.0.113.9"
        );
        assert_eq!(
            client_ip_from(cloudfront, &headers("6.6.6.6, 203.0.113.9"), None),
            "203.0.113.9"
        );
        assert_eq!(
            client_ip_from(None, &headers("6.6.6.6, 203.0.113.9"), None),
            "203.0.113.9"
        );
        // A direct call: its source address.
        assert_eq!(
            client_ip_from(Some("198.51.100.4"), &axum::http::HeaderMap::new(), None),
            "198.51.100.4"
        );
        assert_eq!(
            client_ip_from(None, &axum::http::HeaderMap::new(), None),
            "unknown"
        );
        let mut real_ip = axum::http::HeaderMap::new();
        real_ip.insert("x-real-ip", "6.6.6.6".parse().unwrap());
        assert_eq!(client_ip_from(None, &real_ip, None), "unknown");
    }

    #[test]
    fn forwarded_for_is_read_only_with_the_origin_secret() {
        let secret = Some("origin-secret-value");
        let cloudfront = Some("130.176.1.1");
        // Through CloudFront: the header matches, the viewer's hop is read.
        assert_eq!(
            client_ip_from(
                cloudfront,
                &forwarded(
                    "6.6.6.6, 203.0.113.9, 130.176.1.1",
                    Some("origin-secret-value")
                ),
                secret,
            ),
            "203.0.113.9"
        );
        // A direct call to the function URL forging X-Forwarded-For: its
        // source address, whatever the forged entries say.
        let attacker = Some("198.51.100.4");
        for origin in [None, Some("guess"), Some("origin-secret-valuX"), Some("")] {
            assert_eq!(
                client_ip_from(attacker, &forwarded("203.0.113.9", origin), secret),
                "198.51.100.4",
                "{origin:?}"
            );
        }
        assert_eq!(
            client_ip_from(None, &forwarded("203.0.113.9", Some("guess")), secret),
            "unknown"
        );
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
