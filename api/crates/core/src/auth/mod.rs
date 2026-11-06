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
//! over every provider, a pushed CRM notification, the shop's maintenance), on
//! `/v1/sms/{provider}/messages` with `POST`
//! (sending those texts), on [`DEVICE_ACTIONS`] with `POST` (an
//! attachment upload, Improve message, AI reply, the typing suggestion and
//! what it learns, filling a template) and on
//! the Deals tab's changes (`deal_write`: a deal, its to-dos, notes and
//! files; not deleting a deal or the board setup), and a customer's sales
//! rep or invalid lead (`customer_write`: the contact card's and the lead
//! inbox's Assign, the lead notification's Mark invalid), and the To do tab's
//! AI to-do list (`promise_route`: read it, close or reopen one), and the
//! native quote screen (`DEVICE_QUOTE_READS` with `GET`, and `quote_write`:
//! a new quote, its info, deleting and copying it, a saved revision and the
//! autosave, today's prices, the sale, its email and links, a print; and
//! `quote_print_and_templates`: a form's layout and the letterhead to print it,
//! the caller's quote email templates), and the sale form and QuickBooks
//! invoice editor that open after a quote is sold (`DEVICE_SALE_READS` with
//! `GET`, `place_read`, and `sale_and_invoice_write`: the sale, a quick-added
//! stone, the invoice or its split, its draft rules and sync link), and the
//! company's own (non-QuickBooks) invoice made from a quote (`own_invoice`:
//! the next number, creating it, marking it sent once emailed) the
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
//! Every extractor above also applies position access ([`sections`]): when a
//! company limits the tabs of someone's positions, they are refused the data
//! only their closed tabs use, and a change that needs a skill their
//! positions lack ([`skills`]: selling slabs, changing a transaction or a
//! quote, planning the schedule). With limited tabs they pass `EmployeeUser` and
//! `ShopOrEmployeeUser`, and on an admin tab's routes they act as a company
//! admin (`is_admin`, so `AdminUser` and the handlers' own checks agree).

pub mod agent;
pub mod cookie;
pub mod device;
pub mod job;
pub mod sections;
pub mod session;
pub mod skills;
pub mod web_link;

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use cookie::{SESSION_COOKIE_NAME, cookie_values, decode_session};
use sections::Access;
use session::{SessionUser, effective_user, find_session_user, user_positions};

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

/// Who is asking and what their positions open. A request outside the tabs
/// of their positions is refused here, before any handler; on an admin tab
/// of theirs they act as a company admin.
async fn authenticate_with_access(
    parts: &Parts,
    state: &AppState,
) -> Result<(SessionUser, Vec<session::UserPositionRow>, Access), ApiError> {
    let (mut user, positions) = identify(parts, state).await?;
    let access = sections::user_access(&state.pool, &user, &positions).await?;
    let path = parts.uri.path();
    if !access.allows(path) {
        return Err(sections::outside_sections());
    }
    skills::check(&state.pool, &user, &positions, &parts.method, path).await?;
    if access.allows_admin(path) {
        user.is_admin = true;
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
    let path = parts.uri.path();
    if !sections::user_access(&state.pool, user, &positions)
        .await?
        .allows(path)
    {
        return Err(sections::outside_sections());
    }
    skills::check(&state.pool, user, &positions, &parts.method, path).await
}

/// The CRM routes the Granite Phone app also calls with its device token.
///
/// These prefixes and everything under them. `/v1/me/schedule` is the
/// schedule-only app of installers and template makers (their web portal);
/// `/v1/delayed` is the caller's own scheduled texts and emails (list, edit,
/// send now, cancel), as the composers' Send later list has them on the web;
/// `/v1/me/todos` and `/v1/me/todo-order` are the caller's own to-do list
/// (the app's To do tab).
pub const DEVICE_ROUTES: [&str; 7] = [
    "/v1/emails",
    "/v1/schedule",
    "/v1/events",
    "/v1/me/schedule",
    "/v1/delayed",
    "/v1/me/todos",
    "/v1/me/todo-order",
];

/// CRM routes the app reads with its device token, `GET` only (my texts
/// over every provider for the Text tab of a company without Telnyx, the
/// email and text composers' From Stones, From Images, From Documents and
/// Use Template lists, the suggestions under an email's To, Cc and Bcc, the
/// schedule's calendars, and the More screens: Deals, Customers, Stones,
/// Suppliers, Images (with its folders), Transactions and Quotes; the CRM
/// notification a push names; the shop's maintenance checks a push says are
/// due; the customer stones page the typing
/// suggestion offers to link; and the sales reps that Assign a sales rep lists).
/// These prefixes and everything under them; writes there still need the CRM
/// session.
pub const DEVICE_READ_ROUTES: [&str; 19] = [
    "/v1/sms/mine",
    "/v1/ai/compose/inventory-url",
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
    "/v1/shop/maintenance",
    "/v1/users/sales-reps",
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

/// The customer changes the app makes with its device token, exactly these:
/// a customer's sales rep (`PUT /v1/customers/{id}/sales-rep`), from the
/// contact card's Assign and the lead inbox, and marking a lead invalid
/// (`PUT /v1/customers/{id}/invalid`, admins and sales managers only, as on
/// the web), from the lead notification's Mark invalid. Editing or deleting a
/// customer stays with the CRM session.
fn customer_write(method: &Method, path: &str) -> bool {
    method == Method::PUT
        && path.strip_prefix("/v1/customers/").is_some_and(|rest| {
            rest.strip_suffix("/sales-rep")
                .or_else(|| rest.strip_suffix("/invalid"))
                .is_some_and(is_id)
        })
}

/// The app's To do tab, exactly these: the caller's AI to-do list (`GET
/// /v1/ai/promises`, with its filters) and marking one done, not wanted or
/// back on the list (`PATCH /v1/ai/promises/{id}`). The handlers keep them to
/// the caller's own and to reps AI promises is on for.
fn promise_route(method: &Method, path: &str) -> bool {
    match path.strip_prefix("/v1/ai/promises") {
        Some("") => method == Method::GET,
        Some(rest) => method == Method::PATCH && rest.strip_prefix('/').is_some_and(is_id),
        None => false,
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

/// The quote screen's print and Email sheet with the device token: a company
/// form's layout and the company's letterhead (`GET /v1/quote-forms/{id}`,
/// `GET /v1/quote-forms/letterhead`), so the app makes a form's PDF itself,
/// and the caller's own quote email templates (`POST
/// /v1/me/quote-email-templates`, `PUT|DELETE .../{id}`). Every other form
/// change stays with the CRM session.
fn quote_print_and_templates(method: &Method, path: &str) -> bool {
    if let Some(rest) = path.strip_prefix("/v1/quote-forms/") {
        return method == Method::GET && (rest == "letterhead" || is_id(rest));
    }
    match path.strip_prefix("/v1/me/quote-email-templates") {
        Some("") => method == Method::POST,
        Some(rest) => rest
            .strip_prefix('/')
            .is_some_and(|id| is_id(id) && (method == Method::PUT || method == Method::DELETE)),
        None => false,
    }
}

/// The quote lookups the app's native quote screen reads with its device
/// token, exactly these paths and `GET` only: the price lists a quote can move
/// to and a new quote starts from, the tax rates and payment terms of Pricing
/// Settings and Edit Payment Terms, the forms the Email and Print dialogs
/// offer, the caller's quote email templates, the company's features (Mark as
/// Sold's template readiness and job notes, Undo sale's warning), the event
/// templates Mark as Sold offers as Extra events, and whether Google Maps
/// finds the quote's address before it is printed. A price list's editor
/// (`/{id}`) and every write there stay with the CRM session.
pub const DEVICE_QUOTE_READS: [&str; 9] = [
    "/v1/quote-price-lists",
    "/v1/quote-tax-rates",
    "/v1/quote-payment-terms",
    "/v1/quote-forms",
    "/v1/me/quote-email-templates",
    "/v1/company/features",
    // Miles to a lead's address, on the lead card.
    "/v1/company/drive-miles",
    "/v1/quote-sale-events",
    "/v1/places/check",
];

/// The quote changes the app's native quote screen makes with its device
/// token, exactly these: a new quote (`POST /v1/quotes`), its info (`PATCH
/// /v1/quotes/{id}`), deleting it (`DELETE /v1/quotes/{id}`), saving its
/// drawing as a new revision (`PUT .../drawing`), the unsaved drawing kept
/// every 15 seconds and discarded (`PUT|DELETE .../autosave`), Copy quote
/// (`POST .../copies`), Update prices (`POST .../prices`), Mark as Sold and
/// Undo sale (`POST|DELETE .../sale`), emailing it (`POST .../emails`),
/// switching a sent email's link off or on (`PATCH .../emails/{email_id}`)
/// and recording a print (`POST .../prints`). Reading a quote is under
/// [`DEVICE_READ_ROUTES`]; making its QuickBooks invoice is
/// [`sale_and_invoice_write`]; the customer link, contracts and the browser's
/// print previews stay with the CRM session. The
/// `edit_quotes` skill still applies ([`skills`]).
fn quote_write(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/quotes") else {
        return false;
    };
    if rest.is_empty() {
        return method == Method::POST;
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    let (is_post, is_put, is_patch, is_delete) = (
        method == Method::POST,
        method == Method::PUT,
        method == Method::PATCH,
        method == Method::DELETE,
    );
    match parts.as_slice() {
        [id] => is_id(id) && (is_patch || is_delete),
        [id, "drawing"] => is_id(id) && is_put,
        [id, "autosave"] => is_id(id) && (is_put || is_delete),
        [id, "copies" | "prices" | "emails" | "prints"] => is_id(id) && is_post,
        [id, "sale"] => is_id(id) && (is_post || is_delete),
        [id, "emails", email] => is_id(id) && is_id(email) && is_patch,
        _ => false,
    }
}

/// What the app's native sale form and QuickBooks invoice editor read with the
/// device token, exactly these paths and `GET` only, after a quote is sold
/// (Admin › Quotes › Automation "Sale on a win" and "Create invoice from
/// quote"): the sink and faucet models with free units, the slabs picked (by
/// id), the stone types Add New Color offers, the address suggestions of the
/// project address (also the schedule's Location), and the company's
/// QuickBooks products, services and classes and its usual split of a quote's
/// invoice. A place's address (`GET /v1/places/{place_id}`) is
/// [`place_read`]. Every change to the catalogs stays with the CRM session.
pub const DEVICE_SALE_READS: [&str; 7] = [
    "/v1/sinks",
    "/v1/faucets",
    "/v1/slabs",
    "/v1/stone-types",
    "/v1/places/autocomplete",
    "/v1/qbo/items",
    "/v1/qbo/invoice-split",
];

/// `GET /v1/places/{place_id}`: the address (with its ZIP) of a suggestion
/// picked from [`DEVICE_SALE_READS`]' autocomplete.
fn place_read(method: &Method, path: &str) -> bool {
    method == Method::GET
        && path
            .strip_prefix("/v1/places/")
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// The changes the app's native sale form and invoice editor make with the
/// device token, exactly these: the sale (`POST /v1/sales`, which needs the
/// Sell slabs skill, [`skills`]), Add New Color's stone with its slabs (`POST
/// /v1/stones/quick`), the QuickBooks invoice made from the sold option, one
/// or split in several (`POST /v1/customers/{id}/qbo-invoices` and
/// `.../qbo-invoices/split`), the company's usual changes made to its draft
/// (`POST /v1/quotes/{id}/invoice-draft`) and "Keep in sync with the quote"
/// (`PUT /v1/quotes/{id}/invoice-sync`). Editing or cancelling a sale and
/// editing an invoice stay with the CRM session.
fn sale_and_invoice_write(method: &Method, path: &str) -> bool {
    if path == "/v1/sales" || path == "/v1/stones/quick" {
        return method == Method::POST;
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["", "v1", "customers", id, "qbo-invoices"]
        | ["", "v1", "customers", id, "qbo-invoices", "split"] => {
            is_id(id) && method == Method::POST
        }
        ["", "v1", "quotes", id, "invoice-draft"] => is_id(id) && method == Method::POST,
        ["", "v1", "quotes", id, "invoice-sync"] => is_id(id) && method == Method::PUT,
        _ => false,
    }
}

/// The company's own invoice (the CRM keeps it, for a company not on
/// QuickBooks) that the app's native invoice editor makes from a quote,
/// exactly these: the number a new one gets (`GET /v1/invoices`, which also
/// lists them), creating it (`POST /v1/invoices`) and marking it sent once
/// it is emailed (`PUT /v1/invoices/{id}/status`). Opening or editing an
/// invoice stays with the CRM session.
fn own_invoice(method: &Method, path: &str) -> bool {
    if path == "/v1/invoices" {
        return method == Method::GET || method == Method::POST;
    }
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["", "v1", "invoices", id, "status"] => is_id(id) && method == Method::PUT,
        _ => false,
    }
}

/// A request the device token may sign: any method on [`DEVICE_ROUTES`],
/// `GET` on [`DEVICE_READ_ROUTES`], `POST` on [`DEVICE_ACTIONS`] and on a
/// provider's `/v1/sms/{provider}/messages`, and the Deals tab's changes
/// ([`deal_write`]), a customer's sales rep ([`customer_write`]), the
/// templates' ([`template_write`]), the To do tab's ([`promise_route`]) and
/// the quote screen's (`GET` on [`DEVICE_QUOTE_READS`], [`quote_write`],
/// [`quote_print_and_templates`]), and the sale form's and invoice editor's
/// (`GET` on [`DEVICE_SALE_READS`], [`place_read`],
/// [`sale_and_invoice_write`]) and its own invoice ([`own_invoice`]).
fn device_route(method: &Method, path: &str) -> bool {
    DEVICE_ROUTES.iter().any(|prefix| under(path, prefix))
        || (method == Method::GET && DEVICE_READ_ROUTES.iter().any(|prefix| under(path, prefix)))
        || (method == Method::POST && (DEVICE_ACTIONS.contains(&path) || sms_send(path)))
        || deal_write(method, path)
        || customer_write(method, path)
        || template_write(method, path)
        || promise_route(method, path)
        || (method == Method::GET && DEVICE_QUOTE_READS.contains(&path))
        || quote_write(method, path)
        || quote_print_and_templates(method, path)
        || (method == Method::GET && DEVICE_SALE_READS.contains(&path))
        || place_read(method, path)
        || sale_and_invoice_write(method, path)
        || own_invoice(method, path)
}

async fn identify(
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
        // The phone's token on a route it may not use: refused, but not "signed out" (401), which
        // would sign the phone out although its token is good (an app newer than the API, say).
        (None, None) if device::device_token(parts).is_some() => return Err(ApiError::Forbidden),
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
            "/v1/delayed/sms",
            "/v1/delayed/emails",
            "/v1/delayed/sms/7/send-now",
            "/v1/me/todos",
            "/v1/me/todos/done",
            "/v1/me/todo-order",
        ] {
            assert!(device_route(&Method::GET, path), "{path}");
            assert!(device_route(&Method::POST, path), "{path}");
        }
        for path in [
            "/v1/delayed/sms/7",
            "/v1/delayed/emails/7",
            "/v1/me/todos/7",
            "/v1/me/todos/7/done",
        ] {
            assert!(device_route(&Method::PUT, path), "{path}");
            assert!(device_route(&Method::DELETE, path), "{path}");
        }
        for path in [
            "/v1/emailsx",
            "/v1/eventsfoo/1",
            "/v1/me/devices",
            "/v1/me",
            "/v1/me/schedulex",
            "/v1/delayedx",
            "/v1/me/todosx",
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
            "/v1/quotes/7/revisions/3",
            "/v1/notifications/7",
            "/v1/shop/maintenance",
            "/v1/shop/maintenance/checks",
            "/v1/ai/compose/inventory-url",
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
            "/v1/shop/maintenancex",
            "/v1/shop",
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
    fn the_phone_assigns_a_sales_rep() {
        assert!(device_route(&Method::GET, "/v1/users/sales-reps"));
        assert!(device_route(&Method::PUT, "/v1/customers/7/sales-rep"));
        assert!(device_route(&Method::PUT, "/v1/customers/7/invalid"));
        for (method, path) in [
            (Method::POST, "/v1/users/sales-reps"),
            (Method::GET, "/v1/users/employees"),
            (Method::GET, "/v1/users/sales-repsx"),
            (Method::POST, "/v1/customers/7/sales-rep"),
            (Method::PUT, "/v1/customers/x/sales-rep"),
            (Method::PUT, "/v1/customers//sales-rep"),
            (Method::PUT, "/v1/customers/7/8/sales-rep"),
            (Method::PUT, "/v1/customers/7/temperature"),
            (Method::POST, "/v1/customers/7/invalid"),
            (Method::PUT, "/v1/customers/x/invalid"),
            (Method::PUT, "/v1/customers/7/invalidx"),
            (Method::PUT, "/v1/customers/7"),
            (Method::DELETE, "/v1/customers/7"),
        ] {
            assert!(!device_route(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn the_quote_screen_reads_its_lookups_and_makes_exactly_its_changes() {
        for (method, path) in [
            (Method::GET, "/v1/quotes"),
            (Method::GET, "/v1/quotes/7"),
            (Method::GET, "/v1/quotes/7/revisions/3"),
            (Method::GET, "/v1/quotes/suggestions"),
            (Method::GET, "/v1/quote-price-lists"),
            (Method::GET, "/v1/quote-tax-rates"),
            (Method::GET, "/v1/quote-payment-terms"),
            (Method::GET, "/v1/quote-forms"),
            (Method::GET, "/v1/me/quote-email-templates"),
            (Method::POST, "/v1/quotes"),
            (Method::PATCH, "/v1/quotes/7"),
            (Method::PUT, "/v1/quotes/7/drawing"),
            (Method::POST, "/v1/quotes/7/sale"),
            (Method::DELETE, "/v1/quotes/7/sale"),
            (Method::POST, "/v1/quotes/7/emails"),
            (Method::GET, "/v1/company/features"),
            (Method::GET, "/v1/company/drive-miles"),
            (Method::GET, "/v1/quote-sale-events"),
            (Method::GET, "/v1/places/check"),
            (Method::DELETE, "/v1/quotes/7"),
            (Method::PUT, "/v1/quotes/7/autosave"),
            (Method::DELETE, "/v1/quotes/7/autosave"),
            (Method::POST, "/v1/quotes/7/copies"),
            (Method::POST, "/v1/quotes/7/prices"),
            (Method::POST, "/v1/quotes/7/prints"),
            (Method::PATCH, "/v1/quotes/7/emails/3"),
            (Method::GET, "/v1/quote-forms/3"),
            (Method::GET, "/v1/quote-forms/letterhead"),
            (Method::POST, "/v1/me/quote-email-templates"),
            (Method::PUT, "/v1/me/quote-email-templates/3"),
            (Method::DELETE, "/v1/me/quote-email-templates/3"),
        ] {
            assert!(device_route(&method, path), "{method} {path}");
        }
        for (method, path) in [
            (Method::PUT, "/v1/quotes/7"),
            (Method::POST, "/v1/quotes/7"),
            (Method::DELETE, "/v1/quotes"),
            (Method::DELETE, "/v1/quotes/x"),
            (Method::PATCH, "/v1/quotes"),
            (Method::PATCH, "/v1/quotes/x"),
            (Method::PATCH, "/v1/quotes/"),
            (Method::POST, "/v1/quotes/7/drawing"),
            (Method::POST, "/v1/quotes/7/autosave"),
            (Method::PUT, "/v1/quotes/7/autosavex"),
            (Method::PUT, "/v1/quotes/x/autosave"),
            (Method::PUT, "/v1/quotes/7/copies"),
            (Method::DELETE, "/v1/quotes/7/prices"),
            (Method::PATCH, "/v1/quotes/7/prints"),
            (Method::POST, "/v1/quotes/7/portal-token"),
            (Method::PATCH, "/v1/quotes/7/emails"),
            (Method::PATCH, "/v1/quotes/7/emails/x"),
            (Method::DELETE, "/v1/quotes/7/emails/3"),
            (Method::PATCH, "/v1/quotes/7/emails/3/views"),
            (Method::POST, "/v1/quotes/7/view-link"),
            (Method::POST, "/v1/company/features"),
            (Method::POST, "/v1/company/drive-miles"),
            (Method::GET, "/v1/company/drive-milesx"),
            (Method::GET, "/v1/company/featuresx"),
            (Method::GET, "/v1/company/auto-sale"),
            (Method::PUT, "/v1/company/auto-unscheduled-jobs"),
            (Method::POST, "/v1/quote-sale-events"),
            (Method::PUT, "/v1/quote-sale-events/3"),
            (Method::DELETE, "/v1/quote-sale-events/3"),
            (Method::GET, "/v1/quote-sale-events/3"),
            (Method::POST, "/v1/places/check"),
            (Method::PUT, "/v1/quotes/7/sale"),
            (Method::POST, "/v1/quotes/x/sale"),
            (Method::POST, "/v1/quotes/7/emails/3"),
            (Method::POST, "/v1/quotes/7/invoice-sync"),
            (Method::POST, "/v1/quotes/print-previews"),
            (Method::POST, "/v1/quotesx"),
            (Method::POST, "/v1/quote-price-lists"),
            (Method::GET, "/v1/quote-price-lists/7"),
            (Method::PUT, "/v1/quote-price-lists/7"),
            (Method::GET, "/v1/quote-price-listsx"),
            (Method::POST, "/v1/quote-tax-rates"),
            (Method::DELETE, "/v1/quote-payment-terms/3"),
            (Method::PUT, "/v1/quote-forms/3"),
            (Method::DELETE, "/v1/quote-forms/3"),
            (Method::POST, "/v1/quote-forms"),
            (Method::POST, "/v1/quote-forms/3/copies"),
            (Method::PUT, "/v1/quote-forms/order"),
            (Method::GET, "/v1/quote-forms/3/x"),
            (Method::GET, "/v1/quote-forms/x"),
            (Method::GET, "/v1/me/quote-email-templates/3"),
            (Method::PUT, "/v1/me/quote-email-templates"),
            (Method::PATCH, "/v1/me/quote-email-templates/3"),
            (Method::DELETE, "/v1/me/quote-email-templates/x"),
            (Method::POST, "/v1/me/quote-email-templates/3"),
            (Method::GET, "/v1/quote-email-templates"),
            (Method::GET, "/v1/quote-settings"),
            (Method::GET, "/v1/quote-extras"),
        ] {
            assert!(!device_route(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn the_to_do_tab_reads_and_closes_ai_promises() {
        assert!(device_route(&Method::GET, "/v1/ai/promises"));
        assert!(device_route(&Method::PATCH, "/v1/ai/promises/12"));
        for (method, path) in [
            (Method::POST, "/v1/ai/promises"),
            (Method::GET, "/v1/ai/promises/12"),
            (Method::DELETE, "/v1/ai/promises/12"),
            (Method::PATCH, "/v1/ai/promises/x"),
            (Method::PATCH, "/v1/ai/promises/12/draft"),
            (Method::GET, "/v1/ai/promisesx"),
            (Method::GET, "/v1/ai/chat"),
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
    fn the_sale_form_and_invoice_editor_after_a_sold_quote() {
        for (method, path) in [
            (Method::GET, "/v1/sinks"),
            (Method::GET, "/v1/faucets"),
            (Method::GET, "/v1/slabs"),
            (Method::GET, "/v1/stone-types"),
            (Method::GET, "/v1/places/autocomplete"),
            (Method::GET, "/v1/places/ChIJabc123"),
            (Method::GET, "/v1/qbo/items"),
            (Method::GET, "/v1/qbo/invoice-split"),
            (Method::POST, "/v1/sales"),
            (Method::POST, "/v1/stones/quick"),
            (Method::POST, "/v1/customers/7/qbo-invoices"),
            (Method::POST, "/v1/customers/7/qbo-invoices/split"),
            (Method::POST, "/v1/quotes/7/invoice-draft"),
            (Method::PUT, "/v1/quotes/7/invoice-sync"),
            // Read under DEVICE_READ_ROUTES already: the free slabs of a
            // stone, the customer's sales and an invoice's PDF.
            (Method::GET, "/v1/stones/7/slabs"),
            (Method::GET, "/v1/customers/7/sales"),
            (Method::GET, "/v1/customers/7/qbo-invoices/9/pdf"),
            (Method::GET, "/v1/quotes/7/invoice-sync"),
            (Method::GET, "/v1/quotes/inventory-slabs"),
        ] {
            assert!(device_route(&method, path), "{method} {path}");
        }
        for (method, path) in [
            // Editing or cancelling a sale, and the catalogs, stay with the CRM.
            (Method::PUT, "/v1/sales/7"),
            (Method::POST, "/v1/sales/7/unsell"),
            (Method::GET, "/v1/sales"),
            (Method::POST, "/v1/salesx"),
            (Method::POST, "/v1/sinks"),
            (Method::PUT, "/v1/sinks/3"),
            (Method::GET, "/v1/sinks/3"),
            (Method::POST, "/v1/faucets"),
            (Method::PATCH, "/v1/slabs/3"),
            (Method::GET, "/v1/slabs/3"),
            (Method::POST, "/v1/stone-types"),
            (Method::DELETE, "/v1/stone-types/3"),
            (Method::PUT, "/v1/stones/quick"),
            (Method::POST, "/v1/stones"),
            (Method::POST, "/v1/stones/7/slabs"),
            (Method::POST, "/v1/places/autocomplete"),
            (Method::GET, "/v1/places/a/b"),
            (Method::GET, "/v1/places/"),
            // Editing an invoice and the invoice settings stay with the CRM.
            (Method::PUT, "/v1/customers/7/qbo-invoices/9"),
            (Method::POST, "/v1/customers/x/qbo-invoices"),
            (Method::POST, "/v1/customers/7/qbo-invoices/9"),
            (Method::POST, "/v1/qbo/items"),
            (Method::GET, "/v1/qbo/invoice-instructions"),
            (Method::PUT, "/v1/qbo/invoice-instructions"),
            (Method::GET, "/v1/qbo/invoices/9/pdf"),
            (Method::POST, "/v1/ai/invoice-edit"),
            (Method::PUT, "/v1/quotes/7/invoice-draft"),
            (Method::POST, "/v1/quotes/7/invoice-sync"),
            (Method::DELETE, "/v1/quotes/7/invoice-sync"),
            (Method::PUT, "/v1/quotes/x/invoice-sync"),
        ] {
            assert!(!device_route(&method, path), "{method} {path}");
        }
    }

    #[test]
    fn the_own_invoice_editor_makes_a_quotes_invoice() {
        for (method, path) in [
            (Method::GET, "/v1/invoices"),
            (Method::POST, "/v1/invoices"),
            (Method::PUT, "/v1/invoices/7/status"),
        ] {
            assert!(device_route(&method, path), "{method} {path}");
        }
        for (method, path) in [
            // Opening, editing or deleting an invoice stays with the CRM.
            (Method::GET, "/v1/invoices/7"),
            (Method::PUT, "/v1/invoices/7"),
            (Method::DELETE, "/v1/invoices/7"),
            (Method::PUT, "/v1/invoices"),
            (Method::DELETE, "/v1/invoices"),
            (Method::GET, "/v1/invoices/7/status"),
            (Method::POST, "/v1/invoices/7/status"),
            (Method::PUT, "/v1/invoices/x/status"),
            (Method::PUT, "/v1/invoices//status"),
            (Method::PUT, "/v1/invoices/7/status/x"),
            (Method::POST, "/v1/invoicesx"),
            (Method::GET, "/v1/invoices/"),
        ] {
            assert!(!device_route(&method, path), "{method} {path}");
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
