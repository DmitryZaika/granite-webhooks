//! "Port numbers" on the admin Phone page: moving the company's existing
//! numbers (RingCentral, CloudTalk or another carrier) to Telnyx. Session
//! cookie only; company admins, super admins and superusers (`AdminUser`).
//! Every change is written to `phone_audit_log` and the order's timeline.
//!
//! - `POST   /v1/phone/admin/ports/portability`      → can these numbers move
//! - `POST   /v1/phone/admin/ports`                  → a draft port (Telnyx may split it)
//! - `GET    /v1/phone/admin/ports`                  → the company's ports (our copy)
//! - `GET    /v1/phone/admin/ports/{id}`             → one port, read again from Telnyx
//! - `PATCH  /v1/phone/admin/ports/{id}`             → business details, address, port type, date
//! - `DELETE /v1/phone/admin/ports/{id}`             → delete a draft
//! - `POST   /v1/phone/admin/ports/{id}/submit`      → submit (or resubmit after an exception)
//! - `POST   /v1/phone/admin/ports/{id}/cancel`      → ask Telnyx to cancel
//! - `GET    /v1/phone/admin/ports/{id}/requirements`, `/foc-windows`, `/loa-template`
//! - `POST   /v1/phone/admin/ports/{id}/documents`   → the signed LOA, the bill
//! - `GET    /v1/phone/admin/ports/{id}/timeline`, `GET|POST .../comments`
//!
//! Telnyx's refusals come back as 400 with its reason, phone numbers
//! masked (error texts reach error reporting).

use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::company::{self, CompanyConfig};
use crate::phone::numbers::{is_nanp, to_e164};
use crate::phone::porting::{
    self as port, MAX_NUMBERS, PortRow, PortStatus, TimelineRow, audit, record, setup_changes,
};
use crate::phone::wire_enum;
use crate::serde_helpers::{js_date, js_date_required};
use crate::state::AppState;
use crate::telnyx::porting::{FocWindow, PortingError, PortingOrder, foc_time};
use axum::Json;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use lambda_http::tracing;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::LazyLock;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

/// The largest document (once decoded).
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
/// The upload route's body limit: a 4 MB file in base64, and the JSON around it.
const UPLOAD_BODY_BYTES: usize = 6 * 1024 * 1024;
/// Longest label, comment and text value.
const LABEL_CHARS: usize = 100;
const COMMENT_CHARS: usize = 2000;
const TEXT_CHARS: usize = 100;
const VALUE_CHARS: usize = 500;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(check_port_portability))
        .routes(routes!(list_port_orders, create_port_order))
        .routes(routes!(
            get_port_order,
            update_port_order,
            delete_port_order
        ))
        .routes(routes!(submit_port_order))
        .routes(routes!(cancel_port_order))
        .routes(routes!(list_port_requirements))
        .routes(routes!(list_port_foc_windows))
        .routes(routes!(download_port_loa_template))
        .routes(routes!(list_port_timeline))
        .routes(routes!(list_port_comments, add_port_comment))
        .merge(
            OpenApiRouter::new()
                .routes(routes!(upload_port_document))
                .layer(DefaultBodyLimit::max(UPLOAD_BODY_BYTES)),
        )
}

/// One of Telnyx's reasons for an order's status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortStatusDetail {
    /// Telnyx's code, e.g. `ACCOUNT_NUMBER_MISMATCH`, `LOA_ILLEGIBLE`,
    /// `FOC_REJECTED`.
    pub code: String,
    /// What it means, in Telnyx's words; null when Telnyx gave none.
    pub description: Option<String>,
}

/// What the admin may do with a port now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortActions {
    /// Change its details, documents or date (`PATCH`, documents).
    pub edit: bool,
    /// Submit it (`POST .../submit`): a draft, or again after an exception.
    pub submit: bool,
    /// Ask Telnyx to cancel it (`POST .../cancel`); false within 48 hours of
    /// the confirmed port date (Telnyx support must do it then).
    pub cancel: bool,
    /// Delete it (`DELETE`): drafts only.
    pub delete: bool,
    /// Write to Telnyx's porting team (`POST .../comments`).
    pub comment: bool,
}

/// A port order: our copy, as the Phone page lists it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortOrder {
    /// Our id (`/v1/phone/admin/ports/{id}`).
    pub id: i64,
    /// The request it came from: orders Telnyx split from one request
    /// share it.
    pub request_id: String,
    /// Where the port is.
    pub status: PortStatus,
    /// Telnyx's reasons for the status (an exception's problems); may be
    /// empty.
    pub status_details: Vec<PortStatusDetail>,
    /// The numbers it moves, E.164.
    pub phone_numbers: Vec<String>,
    /// What the numbers are called once ported; may be empty.
    pub label: String,
    /// What to quote to Telnyx support; null while a draft.
    pub support_key: Option<String>,
    /// The current (losing) carrier, as Telnyx names it; null until known.
    pub old_carrier: Option<String>,
    /// `local`, `toll_free`, `mobile`, ...; null until known.
    pub number_type: Option<String>,
    /// FastPort: it can port within the day of the date.
    pub fast_port: bool,
    /// The port date asked for (UTC); null until picked.
    #[serde(serialize_with = "js_date")]
    pub foc_requested_at: Option<DateTime<Utc>>,
    /// The port date the current carrier confirmed (UTC); null until then.
    #[serde(serialize_with = "js_date")]
    pub foc_at: Option<DateTime<Utc>>,
    /// Telnyx approved every requirement (documents, details).
    pub requirements_met: bool,
    /// Texting's own port: `not_applicable`, `pending`, `activating`,
    /// `exception`, `ported`, ...; null when Telnyx did not say.
    pub messaging_status: Option<String>,
    /// Who started it (name, else email); null when unknown.
    pub created_by_name: Option<String>,
    /// When it was started.
    #[serde(serialize_with = "js_date_required")]
    pub created_at: DateTime<Utc>,
    /// When our copy last changed.
    #[serde(serialize_with = "js_date_required")]
    pub updated_at: DateTime<Utc>,
    /// When it was submitted; null before.
    #[serde(serialize_with = "js_date")]
    pub submitted_at: Option<DateTime<Utc>>,
    /// When Telnyx said it ported; null before.
    #[serde(serialize_with = "js_date")]
    pub ported_at: Option<DateTime<Utc>>,
    /// When the ported numbers were added to the company and set up for
    /// calls; null until then.
    #[serde(serialize_with = "js_date")]
    pub completed_at: Option<DateTime<Utc>>,
    /// What went wrong last on our side (setting up ported numbers), for
    /// the admin; null when nothing.
    pub last_error: Option<String>,
    /// When it was last read from Telnyx; null when never.
    #[serde(serialize_with = "js_date")]
    pub refreshed_at: Option<DateTime<Utc>>,
    /// What the admin may do now.
    pub actions: PortActions,
}

/// `GET /v1/phone/admin/ports`, and the answer of `POST /v1/phone/admin/ports`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortOrders {
    /// Newest first (a new request's orders, in Telnyx's order).
    pub orders: Vec<PortOrder>,
}

/// The current account holder at the losing carrier, as on its bill.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortEndUser {
    /// The business (or person) the account is in the name of; null until set.
    pub entity_name: Option<String>,
    /// Who authorizes the port (signs the LOA); null until set.
    pub auth_person_name: Option<String>,
    /// The account's billing telephone number (BTN); null until set.
    pub billing_phone_number: Option<String>,
    /// The account number at the losing carrier; null until set.
    pub account_number: Option<String>,
    /// A port-out PIN or passcode is saved (it is never sent back).
    pub pin_set: bool,
}

/// The service address on the account.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortLocation {
    /// Street and number; null until set.
    pub street_address: Option<String>,
    /// Suite, floor; null when none.
    pub extended_address: Option<String>,
    /// City; null until set.
    pub locality: Option<String>,
    /// State or province, 2 letters; null until set.
    pub administrative_area: Option<String>,
    /// ZIP or postal code; null until set.
    pub postal_code: Option<String>,
    /// `US` or `CA`; null until set.
    pub country_code: Option<String>,
}

/// Whether every number of the account moves.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortTypeInfo {
    /// `full` (every number of the account) or `partial`; null until set.
    pub kind: Option<String>,
    /// For a partial port, what happens to the numbers left behind: `keep`
    /// or `disconnect`; null otherwise.
    pub remaining_numbers_action: Option<String>,
    /// For a partial port moving the billing number: the account's new one;
    /// null otherwise.
    pub new_billing_phone_number: Option<String>,
}

/// Which of the two documents every port needs are attached.
#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct PortDocuments {
    /// The signed Letter of Authorization.
    pub loa: bool,
    /// A recent bill of the current carrier.
    pub invoice: bool,
}

/// A number of the order, with Telnyx's view of it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortNumber {
    /// The number, E.164.
    pub e164: String,
    /// Telnyx's activation status (`New`, `Pending`, `Conflict`, `Active`,
    /// ...); null when not given.
    pub activation_status: Option<String>,
    /// `pending`, `confirmed` or `provisional`; null when not given.
    pub portability_status: Option<String>,
}

/// What Telnyx has on the order now.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortDetails {
    /// The account holder (business details).
    pub end_user: PortEndUser,
    /// The service address.
    pub location: PortLocation,
    /// Full or partial port.
    pub port_type: PortTypeInfo,
    /// Which documents are attached.
    pub documents: PortDocuments,
    /// The numbers with Telnyx's statuses; empty in answers to changes
    /// (the numbers are in `order.phoneNumbers`).
    pub numbers: Vec<PortNumber>,
    /// Telnyx can move the numbers' texting too; null when it did not say.
    pub messaging_capable: Option<bool>,
}

/// One port, with what Telnyx has on it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortOrderDetail {
    /// Our copy, brought up to date.
    pub order: PortOrder,
    /// Telnyx's details; null when Telnyx could not be read (`refreshError`).
    pub details: Option<PortDetails>,
    /// Why the order could not be read from Telnyx just now; null when it was.
    pub refresh_error: Option<String>,
}

fn order_of(row: &PortRow) -> PortOrder {
    let now = Utc::now();
    PortOrder {
        id: row.id,
        request_id: row.request_id.clone(),
        status: row.status,
        status_details: row
            .details()
            .into_iter()
            .map(|(code, description)| PortStatusDetail { code, description })
            .collect(),
        phone_numbers: row.numbers(),
        label: row.label.clone(),
        support_key: row.support_key.clone(),
        old_carrier: row.old_carrier.clone(),
        number_type: row.number_type.clone(),
        fast_port: row.fast_port,
        foc_requested_at: row.foc_requested_at,
        foc_at: row.foc_at,
        requirements_met: row.requirements_met,
        messaging_status: row.messaging_status.clone(),
        created_by_name: row.created_by_name.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        submitted_at: row.submitted_at,
        ported_at: row.ported_at,
        completed_at: row.completed_at,
        last_error: row.last_error.clone(),
        refreshed_at: row.refreshed_at,
        actions: PortActions {
            edit: row.status.editable(),
            submit: row.status.submittable(),
            cancel: row.status.cancellable() && !row.too_close_to_cancel(now),
            delete: row.status == PortStatus::Draft,
            comment: row.status != PortStatus::Deleted,
        },
    }
}

fn details_of(order: &PortingOrder) -> PortDetails {
    let misc = order.misc.clone().unwrap_or_default();
    PortDetails {
        end_user: PortEndUser {
            entity_name: order.end_user.entity_name.clone(),
            auth_person_name: order.end_user.auth_person_name.clone(),
            billing_phone_number: order
                .end_user
                .billing_phone_number
                .as_deref()
                .map(|number| to_e164(number).unwrap_or_else(|| number.to_string())),
            account_number: order.end_user.account_number.clone(),
            pin_set: order.end_user.has_pin,
        },
        location: PortLocation {
            street_address: order.location.street_address.clone(),
            extended_address: order.location.extended_address.clone(),
            locality: order.location.locality.clone(),
            administrative_area: order.location.administrative_area.clone(),
            postal_code: order.location.postal_code.clone(),
            country_code: order.location.country_code.clone(),
        },
        port_type: PortTypeInfo {
            kind: misc.port_type,
            remaining_numbers_action: misc.remaining_numbers_action,
            new_billing_phone_number: misc.new_billing_phone_number,
        },
        documents: PortDocuments {
            loa: order.loa_document.is_some(),
            invoice: order.invoice_document.is_some(),
        },
        numbers: order
            .phone_numbers
            .iter()
            .filter_map(|number| {
                Some(PortNumber {
                    e164: to_e164(&number.phone_number)?,
                    activation_status: number.activation_status.clone(),
                    portability_status: number.portability_status.clone(),
                })
            })
            .collect(),
        messaging_capable: order.messaging_capable,
    }
}

fn detail(
    row: &PortRow,
    order: Option<&PortingOrder>,
    refresh_error: Option<String>,
) -> PortOrderDetail {
    PortOrderDetail {
        order: order_of(row),
        details: order.map(details_of),
        refresh_error,
    }
}

/// A Telnyx porting failure for the admin: 400 with Telnyx's reason when it
/// refused, 503 when Telnyx is not set up here, 502 when it did not answer.
fn telnyx_refused(err: &PortingError, what: &'static str) -> ApiError {
    tracing::warn!(error = %err, code = ?err.code(), what, "Telnyx porting request failed");
    match err {
        PortingError::NotConfigured => {
            ApiError::Unavailable("Porting is not set up on the server.".into())
        }
        PortingError::Status {
            status: 400 | 404 | 409 | 422,
            detail,
            ..
        } => ApiError::BadRequest(match detail {
            Some(reason) => format!("Telnyx did not accept this: {reason}"),
            None => "Telnyx did not accept this. Check the details and try again.".into(),
        }),
        _ => ApiError::Upstream("Telnyx did not answer. Try again in a minute.".into()),
    }
}

/// The company's Telnyx setup when ports can be made for it: 400 when its
/// TeXML app is not saved, 503 when Telnyx is not set up on the server.
async fn port_setup(state: &AppState, company_id: i32) -> ApiResult<CompanyConfig> {
    let config = company::load(&state.pool, company_id)
        .await?
        .filter(|config| config.texml_app_id.is_some())
        .ok_or_else(|| {
            ApiError::BadRequest(
                "Telnyx is not set up for this company yet. Ask a superuser.".into(),
            )
        })?;
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Porting is not set up on the server.".into(),
        ));
    }
    Ok(config)
}

async fn company_port(state: &AppState, company_id: i32, id: i64) -> ApiResult<PortRow> {
    port::load(&state.pool, company_id, id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// The typed numbers as E.164, in order and once each: 400 unless 1 to 50
/// US or Canada numbers (the message names no number).
fn checked_numbers(typed: &[String]) -> ApiResult<Vec<String>> {
    let mut numbers: Vec<String> = Vec::new();
    for typed in typed {
        let e164 = to_e164(typed).filter(|e164| is_nanp(e164)).ok_or_else(|| {
            ApiError::BadRequest("Every number must be a US or Canada number (10 digits).".into())
        })?;
        if !numbers.contains(&e164) {
            numbers.push(e164);
        }
    }
    if numbers.is_empty() {
        return Err(ApiError::BadRequest("Enter at least one number.".into()));
    }
    if numbers.len() > MAX_NUMBERS {
        return Err(ApiError::BadRequest(format!(
            "Port at most {MAX_NUMBERS} numbers at a time."
        )));
    }
    Ok(numbers)
}

fn check_label(label: &str) -> ApiResult<String> {
    let label = label.trim();
    if label.chars().count() > LABEL_CHARS {
        return Err(ApiError::BadRequest(format!(
            "Use at most {LABEL_CHARS} characters for the label."
        )));
    }
    Ok(label.to_string())
}

/// Body of `POST /v1/phone/admin/ports/portability`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortabilityInput {
    /// The numbers to check, as typed (10 digits or E.164; US and Canada
    /// only), 1 to 50.
    pub phone_numbers: Vec<String>,
}

/// One number's portability.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortabilityResult {
    /// The number, E.164.
    pub e164: String,
    /// It can move to Telnyx.
    pub portable: bool,
    /// FastPort: it can move within the day.
    pub fast_portable: bool,
    /// Its texting can move too; null when Telnyx did not say.
    pub messaging_capable: Option<bool>,
    /// Why it cannot move (Telnyx's words); null when it can.
    pub reason: Option<String>,
    /// It is a company number on Telnyx already (nothing to port).
    pub on_telnyx: bool,
    /// It is in an open port already.
    pub in_open_port: bool,
}

/// `POST /v1/phone/admin/ports/portability`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortabilityResults {
    /// One per distinct number, in the order typed.
    pub results: Vec<PortabilityResult>,
}

/// Check whether numbers can move to Telnyx
///
/// Asks Telnyx whether each number can be ported in (and with FastPort,
/// within the day), and says which are company numbers on Telnyx already or
/// in an open port. Nothing is saved. 400 unless 1 to 50 US or Canada
/// numbers; 503 when Telnyx is not set up on the server; 502 when Telnyx
/// fails. Next: `POST /v1/phone/admin/ports` with the portable ones.
/// Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports/portability",
    operation_id = "check_phone_port_portability",
    tag = "phone",
    request_body = PortabilityInput,
    responses(
        (status = 200, description = "Each number's portability", body = PortabilityResults),
        (status = 400, description = "No numbers, too many, or one that is not a US or Canada number; or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn check_port_portability(
    State(state): State<AppState>,
    AdminUser(_user): AdminUser,
    JsonBody(input): JsonBody<PortabilityInput>,
) -> ApiResult<Json<PortabilityResults>> {
    let numbers = checked_numbers(&input.phone_numbers)?;
    let checked = state
        .telnyx
        .portability_check(&numbers)
        .await
        .map_err(|err| telnyx_refused(&err, "check portability"))?;
    let known = port::on_telnyx(&state.pool, &numbers).await?;
    let open = port::in_open_ports(&state.pool, &numbers).await?;
    let results = numbers
        .into_iter()
        .map(|e164| {
            let verdict = checked
                .iter()
                .find(|verdict| to_e164(&verdict.phone_number).as_deref() == Some(e164.as_str()));
            PortabilityResult {
                portable: verdict.is_some_and(|verdict| verdict.portable),
                fast_portable: verdict.is_some_and(|verdict| verdict.fast_portable),
                messaging_capable: verdict.and_then(|verdict| verdict.messaging_capable),
                reason: match verdict {
                    Some(verdict) if !verdict.portable => Some(
                        verdict
                            .not_portable_reason
                            .clone()
                            .unwrap_or_else(|| "Telnyx cannot port this number.".into()),
                    ),
                    Some(_) => None,
                    None => Some("Telnyx gave no answer for this number.".into()),
                },
                on_telnyx: known.contains(&e164),
                in_open_port: open.contains(&e164),
                e164,
            }
        })
        .collect();
    Ok(Json(PortabilityResults { results }))
}

/// Body of `POST /v1/phone/admin/ports`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortCreateInput {
    /// The numbers to move, as typed (10 digits or E.164; US and Canada
    /// only), 1 to 50. None may be a company number already or in an open
    /// port.
    pub phone_numbers: Vec<String>,
    /// What the numbers are called once ported, up to 100 characters; may
    /// be empty.
    #[serde(default)]
    pub label: String,
}

/// Start porting numbers to Telnyx
///
/// Creates a draft port order in Telnyx for the numbers (Telnyx splits it
/// into several when they are with different carriers, of different types,
/// or FastPort and not: each order is completed and submitted on its own),
/// sets each one up (our webhook; once ported the numbers land on the
/// company's TeXML app, and its messaging profile when texting is set up)
/// and saves our copy. Returns the new orders (201). Next: `PATCH` each
/// with the business details, upload the bill and the signed LOA, pick a
/// date, then `POST .../submit`. 400 when a number is not a US or Canada
/// number, is a company number already or is in an open port, or Telnyx
/// refuses (its reason in the message); 400 too when Telnyx is not set up
/// for the company. Audited. Company admins (and super admins, superusers)
/// only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports",
    operation_id = "create_phone_port_order",
    tag = "phone",
    request_body = PortCreateInput,
    responses(
        (status = 201, description = "The new draft orders", body = PortOrders),
        (status = 400, description = "Bad numbers or label, numbers on Telnyx or in a port already, Telnyx not set up for the company, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<PortCreateInput>,
) -> ApiResult<(StatusCode, Json<PortOrders>)> {
    let pool = &state.pool;
    let numbers = checked_numbers(&input.phone_numbers)?;
    let label = check_label(&input.label)?;
    let config = port_setup(&state, user.company_id).await?;
    let known = port::on_telnyx(pool, &numbers).await?;
    if !known.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "{} of these numbers {} on Telnyx already.",
            known.len(),
            if known.len() == 1 { "is" } else { "are" }
        )));
    }
    let open = port::in_open_ports(pool, &numbers).await?;
    if !open.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "{} of these numbers {} in a port already.",
            open.len(),
            if open.len() == 1 { "is" } else { "are" }
        )));
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let reference = format!("granite-c{}-{}", user.company_id, &request_id[..8]);
    let orders = state
        .telnyx
        .create_porting_orders(
            &numbers,
            &reference,
            &format!("granite-c{}", user.company_id),
        )
        .await
        .map_err(|err| telnyx_refused(&err, "create porting order"))?;
    let profile = port::messaging_profile_id(pool, user.company_id).await?;
    let hook = port::webhook_url(&state, &config);
    let mut rows = Vec::new();
    for order in &orders {
        let id = port::insert(
            pool,
            user.company_id,
            &request_id,
            &label,
            Some(user.id),
            order,
        )
        .await?;
        let status = PortStatus::parse(&order.status).unwrap_or(PortStatus::Draft);
        record(
            pool,
            user.company_id,
            Some(id),
            "created",
            Some(status),
            None,
            Some(user.id),
        )
        .await?;
        audit(
            pool,
            user.company_id,
            Some(user.id),
            "port_created",
            id,
            &json!({ "requestId": request_id, "telnyxOrderId": order.id,
                     "numbers": port::order_numbers(order).len(), "split": orders.len() > 1 }),
        )
        .await?;
        let mut row = company_port(&state, user.company_id, id).await?;
        let changes = setup_changes(&config, &hook, profile.as_deref(), order.messaging_capable);
        match state.telnyx.update_porting_order(&order.id, &changes).await {
            Ok(updated) => row = port::sync(&state, &row, &updated, Some(user.id)).await?,
            Err(err) => {
                tracing::warn!(error = %err, order_id = id, "port order not set up on Telnyx yet");
                port::set_error(
                    pool,
                    id,
                    "Telnyx did not take the order's setup yet; it is sent again with the next change.",
                )
                .await?;
                row = company_port(&state, user.company_id, id).await?;
            }
        }
        rows.push(row);
    }
    Ok((
        StatusCode::CREATED,
        Json(PortOrders {
            orders: rows.iter().map(order_of).collect(),
        }),
    ))
}

/// List the company's number ports
///
/// Every port order of the company, newest first, from our copy (no Telnyx
/// call): numbers, status, port dates, who started it and what the admin
/// may do. Open one with `GET /v1/phone/admin/ports/{id}` to read it again
/// from Telnyx. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports",
    operation_id = "list_phone_port_orders",
    tag = "phone",
    responses(
        (status = 200, description = "The company's port orders", body = PortOrders),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_port_orders(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<PortOrders>> {
    let rows = port::list(&state.pool, user.company_id).await?;
    Ok(Json(PortOrders {
        orders: rows.iter().map(order_of).collect(),
    }))
}

/// Open a number port
///
/// Reads the order from Telnyx and brings our copy up to date (a status
/// change is written to the timeline and told to the admins once; a port
/// that just completed has its numbers added to the company). Returns our
/// copy with Telnyx's details (business details, address, port type,
/// attached documents, the numbers' statuses). When Telnyx cannot be read,
/// the saved copy comes back with `details` null and `refreshError` saying
/// why. 404 when it is not the company's. Company admins (and super admins,
/// superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}",
    operation_id = "get_phone_port_order",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The order, read again from Telnyx", body = PortOrderDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortOrderDetail>> {
    let row = company_port(&state, user.company_id, id).await?;
    if row.status == PortStatus::Deleted {
        return Ok(Json(detail(&row, None, None)));
    }
    match state.telnyx.get_porting_order(&row.telnyx_order_id).await {
        Ok(order) => {
            let row = port::sync(&state, &row, &order, None).await?;
            Ok(Json(detail(&row, Some(&order), None)))
        }
        Err(err) if err.status() == Some(404) && row.status == PortStatus::Draft => {
            port::mark_deleted(&state, &row, None).await?;
            let row = company_port(&state, user.company_id, id).await?;
            Ok(Json(detail(
                &row,
                None,
                Some("Telnyx deleted this draft.".into()),
            )))
        }
        Err(err) => {
            let reason = telnyx_refused(&err, "read porting order").to_string();
            Ok(Json(detail(&row, None, Some(reason))))
        }
    }
}

/// The account holder, as on the current carrier's bill.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortEndUserInput {
    /// The business (or person) the account is in the name of, as on the
    /// bill; up to 100 characters.
    pub entity_name: String,
    /// Who authorizes the port and signs the LOA; up to 100 characters.
    pub auth_person_name: String,
    /// The account's billing telephone number (BTN), a US or Canada number.
    pub billing_phone_number: String,
    /// The account number at the current carrier, as on the bill; up to
    /// 100 characters.
    pub account_number: String,
    /// The port-out PIN or passcode, when the carrier has one (RingCentral
    /// and most wireless carriers do); up to 20 characters. Null or empty
    /// keeps the saved one.
    #[serde(default)]
    pub pin_passcode: Option<String>,
}

/// The service address on the account.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortLocationInput {
    /// Street and number; up to 100 characters.
    pub street_address: String,
    /// Suite, floor; null or empty when none.
    #[serde(default)]
    pub extended_address: Option<String>,
    /// City; up to 100 characters.
    pub locality: String,
    /// State or province, 2 letters (`IN`, `ON`).
    pub administrative_area: String,
    /// ZIP code (`46204`, `46204-1234`) or Canadian postal code (`K1A 0B1`).
    pub postal_code: String,
    /// `US` or `CA`.
    pub country_code: String,
}

wire_enum! {
    /// Whether every number of the account moves.
    pub enum PortKind {
        /// Every number on the account moves (the account closes).
        Full = "full",
        /// Some numbers stay with the current carrier.
        Partial = "partial",
    }
}

wire_enum! {
    /// What happens to the numbers that stay at the current carrier.
    pub enum RemainingNumbers {
        /// They stay in service there.
        Keep = "keep",
        /// They are disconnected.
        Disconnect = "disconnect",
    }
}

/// Full or partial port.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortTypeInput {
    /// `full` or `partial`.
    pub kind: PortKind,
    /// For `partial`: what happens to the numbers that stay; required then.
    #[serde(default)]
    pub remaining_numbers_action: Option<RemainingNumbers>,
    /// For `partial` with `keep` when the billing number is one of those
    /// moving: the account's new billing number (one that stays), a US or
    /// Canada number; required then.
    #[serde(default)]
    pub new_billing_phone_number: Option<String>,
}

/// A text value for one of the order's requirements.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortRequirementInput {
    /// The requirement (`PortRequirement.requirementTypeId`).
    pub requirement_type_id: String,
    /// Its value, up to 500 characters.
    pub field_value: String,
}

/// Body of `PATCH /v1/phone/admin/ports/{id}`: the sections to change; the
/// ones left out (or null) stay as they are.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortUpdateInput {
    /// What the numbers are called once ported (ours only), up to 100
    /// characters.
    #[serde(default)]
    pub label: Option<String>,
    /// The account holder, every field.
    #[serde(default)]
    pub end_user: Option<PortEndUserInput>,
    /// The service address, every field.
    #[serde(default)]
    pub location: Option<PortLocationInput>,
    /// Full or partial port.
    #[serde(default)]
    pub port_type: Option<PortTypeInput>,
    /// The port date to ask for (RFC 3339): inside one of the windows of
    /// `GET .../foc-windows`.
    #[serde(default)]
    pub foc_datetime_requested: Option<DateTime<Utc>>,
    /// Text values for the order's textual requirements, at most 20.
    #[serde(default)]
    pub requirements: Option<Vec<PortRequirementInput>>,
}

fn required(value: &str, name: &str, chars: usize) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ApiError::BadRequest(format!("Enter the {name}.")));
    }
    if value.chars().count() > chars {
        return Err(ApiError::BadRequest(format!(
            "Use at most {chars} characters for the {name}."
        )));
    }
    Ok(value.to_string())
}

fn nanp(value: &str, name: &str) -> ApiResult<String> {
    to_e164(value)
        .filter(|e164| is_nanp(e164))
        .ok_or_else(|| ApiError::BadRequest(format!("The {name} must be a US or Canada number.")))
}

static ZIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{5}(-\d{4})?$").expect("valid pattern"));
static CANADIAN_POSTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z]\d[A-Za-z][ -]?\d[A-Za-z]\d$").expect("valid pattern"));

/// The changes to send Telnyx for the input (`None` for a label-only
/// change), and the names of the sections changed (audit, timeline).
async fn changes_for(
    state: &AppState,
    row: &PortRow,
    input: &PortUpdateInput,
) -> ApiResult<(Value, Vec<&'static str>)> {
    let mut changes = json!({});
    let mut sections = Vec::new();
    let numbers = row.numbers();
    let mut billing_number: Option<String> = None;
    if let Some(end_user) = &input.end_user {
        let billing = nanp(&end_user.billing_phone_number, "billing number")?;
        let mut admin = json!({
            "entity_name": required(&end_user.entity_name, "business name", TEXT_CHARS)?,
            "auth_person_name": required(&end_user.auth_person_name, "authorized person", TEXT_CHARS)?,
            "billing_phone_number": billing,
            "account_number": required(&end_user.account_number, "account number", TEXT_CHARS)?,
        });
        if let Some(pin) = end_user
            .pin_passcode
            .as_deref()
            .map(str::trim)
            .filter(|pin| !pin.is_empty())
        {
            if pin.chars().count() > 20 {
                return Err(ApiError::BadRequest(
                    "Use at most 20 characters for the PIN.".into(),
                ));
            }
            admin["pin_passcode"] = json!(pin);
        }
        billing_number = Some(billing);
        changes["end_user"] = json!({ "admin": admin });
        sections.push("business details");
    }
    if let Some(location) = &input.location {
        let country = location.country_code.trim().to_uppercase();
        if country != "US" && country != "CA" {
            return Err(ApiError::BadRequest(
                "The address must be in the US or Canada.".into(),
            ));
        }
        let area = location.administrative_area.trim().to_uppercase();
        if area.len() != 2 || !area.chars().all(|c| c.is_ascii_uppercase()) {
            return Err(ApiError::BadRequest(
                "Enter the state or province as 2 letters.".into(),
            ));
        }
        let postal = location.postal_code.trim().to_uppercase();
        let postal_ok = if country == "US" {
            ZIP.is_match(&postal)
        } else {
            CANADIAN_POSTAL.is_match(&postal)
        };
        if !postal_ok {
            return Err(ApiError::BadRequest(
                "Enter a valid ZIP or postal code.".into(),
            ));
        }
        let extended = location
            .extended_address
            .as_deref()
            .map(str::trim)
            .filter(|extended| !extended.is_empty());
        if extended.is_some_and(|extended| extended.chars().count() > TEXT_CHARS) {
            return Err(ApiError::BadRequest(format!(
                "Use at most {TEXT_CHARS} characters for the suite or floor."
            )));
        }
        changes["end_user"]["location"] = json!({
            "street_address": required(&location.street_address, "street address", TEXT_CHARS)?,
            "extended_address": extended,
            "locality": required(&location.locality, "city", TEXT_CHARS)?,
            "administrative_area": area,
            "postal_code": postal,
            "country_code": country,
        });
        sections.push("service address");
    }
    if let Some(port_type) = &input.port_type {
        let misc = match port_type.kind {
            PortKind::Full => json!({ "type": "full" }),
            PortKind::Partial => {
                let action = port_type.remaining_numbers_action.ok_or_else(|| {
                    ApiError::BadRequest(
                        "Say what happens to the numbers that stay with the current carrier."
                            .into(),
                    )
                })?;
                let new_billing = match port_type
                    .new_billing_phone_number
                    .as_deref()
                    .map(str::trim)
                    .filter(|number| !number.is_empty())
                {
                    Some(number) => Some(nanp(number, "new billing number")?),
                    None => None,
                };
                let billing = match billing_number.clone() {
                    Some(billing) => Some(billing),
                    None => state
                        .telnyx
                        .get_porting_order(&row.telnyx_order_id)
                        .await
                        .ok()
                        .and_then(|order| order.end_user.billing_phone_number)
                        .and_then(|number| to_e164(&number)),
                };
                let billing_moves = billing.is_some_and(|billing| numbers.contains(&billing));
                if action == RemainingNumbers::Keep && billing_moves && new_billing.is_none() {
                    return Err(ApiError::BadRequest(
                        "The billing number is moving: enter the account's new billing number."
                            .into(),
                    ));
                }
                if new_billing
                    .as_ref()
                    .is_some_and(|number| numbers.contains(number))
                {
                    return Err(ApiError::BadRequest(
                        "The new billing number must be one that stays with the current carrier."
                            .into(),
                    ));
                }
                json!({ "type": "partial", "remaining_numbers_action": action.as_str(),
                        "new_billing_phone_number": new_billing })
            }
        };
        changes["misc"] = misc;
        sections.push("port type");
    }
    if let Some(at) = input.foc_datetime_requested {
        if at <= Utc::now() {
            return Err(ApiError::BadRequest(
                "Pick a port date in the future.".into(),
            ));
        }
        let windows = state
            .telnyx
            .allowed_foc_windows(&row.telnyx_order_id)
            .await
            .map_err(|err| telnyx_refused(&err, "list FOC windows"))?;
        if !windows
            .iter()
            .any(|window| window.started_at <= at && at <= window.ended_at)
        {
            return Err(ApiError::BadRequest(
                "Pick one of the port dates Telnyx offers.".into(),
            ));
        }
        changes["activation_settings"] = json!({ "foc_datetime_requested": foc_time(at) });
        sections.push("port date");
    }
    if let Some(requirements) = &input.requirements {
        if requirements.len() > 20 {
            return Err(ApiError::BadRequest("Send at most 20 requirements.".into()));
        }
        let mut values = Vec::new();
        for requirement in requirements {
            let id = requirement.requirement_type_id.trim();
            if id.is_empty() || id.len() > 64 {
                return Err(ApiError::BadRequest("Unknown requirement.".into()));
            }
            values.push(json!({
                "requirement_type_id": id,
                "field_value": required(&requirement.field_value, "requirement's value", VALUE_CHARS)?,
            }));
        }
        if !values.is_empty() {
            changes["requirements"] = Value::Array(values);
            sections.push("requirements");
        }
    }
    Ok((changes, sections))
}

/// Merges `extra` into `changes` (objects merge, the rest replaces).
fn merge(changes: &mut Value, extra: Value) {
    match (changes, extra) {
        (Value::Object(base), Value::Object(extra)) => {
            for (key, value) in extra {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, extra) => *base = extra,
    }
}

/// Sends `changes` (with the order's setup) to Telnyx and brings our copy
/// up to date: the order as Telnyx has it now.
async fn send_changes(
    state: &AppState,
    row: &PortRow,
    config: &CompanyConfig,
    changes: Value,
    actor: i32,
    what: &'static str,
) -> ApiResult<(PortRow, PortingOrder)> {
    let profile = port::messaging_profile_id(&state.pool, row.company_id).await?;
    let mut body = setup_changes(
        config,
        &port::webhook_url(state, config),
        profile.as_deref(),
        None,
    );
    merge(&mut body, changes);
    let order = state
        .telnyx
        .update_porting_order(&row.telnyx_order_id, &body)
        .await
        .map_err(|err| telnyx_refused(&err, what))?;
    let row = port::sync(state, row, &order, Some(actor)).await?;
    Ok((row, order))
}

/// Change a number port
///
/// Saves the sections sent (each whole; the ones left out stay): the label
/// (ours only), the account holder as on the current carrier's bill
/// (business name, authorized person, billing number, account number, PIN;
/// an empty PIN keeps the saved one), the service address (US or Canada),
/// full or partial port, the port date to ask for (inside a window of `GET
/// .../foc-windows`) and text values for requirements. Telnyx takes changes
/// while the order is a draft, in process or in exception (else 409).
/// Returns the order with Telnyx's details. 400 with what is wrong, or
/// Telnyx's reason when it refuses. Audited (section names only). Company
/// admins (and super admins, superusers) only.
#[utoipa::path(
    patch,
    path = "/v1/phone/admin/ports/{id}",
    operation_id = "update_phone_port_order",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    request_body = PortUpdateInput,
    responses(
        (status = 200, description = "The order as saved", body = PortOrderDetail),
        (status = 400, description = "What is wrong (a missing or bad field, a date Telnyx does not offer), or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "The order can no longer be changed", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<PortUpdateInput>,
) -> ApiResult<Json<PortOrderDetail>> {
    let pool = &state.pool;
    let row = company_port(&state, user.company_id, id).await?;
    if !row.status.editable() {
        return Err(ApiError::Conflict(
            "This port can no longer be changed.".into(),
        ));
    }
    let label = input.label.as_deref().map(check_label).transpose()?;
    let config = port_setup(&state, user.company_id).await?;
    let (changes, mut sections) = changes_for(&state, &row, &input).await?;
    if let Some(label) = &label {
        sqlx::query("UPDATE phone_port_orders SET label = ? WHERE id = ?")
            .bind(label)
            .bind(row.id)
            .execute(pool)
            .await?;
        sections.push("label");
    }
    if sections.is_empty() {
        return Err(ApiError::BadRequest("Nothing to change.".into()));
    }
    let row = company_port(&state, user.company_id, id).await?;
    let (row, order) = if changes
        .as_object()
        .is_some_and(|changes| !changes.is_empty())
    {
        let (row, order) = send_changes(
            &state,
            &row,
            &config,
            changes,
            user.id,
            "update porting order",
        )
        .await?;
        (row, Some(order))
    } else {
        (row, None)
    };
    let saved = sections.join(", ");
    record(
        pool,
        user.company_id,
        Some(id),
        "details",
        None,
        Some(&saved),
        Some(user.id),
    )
    .await?;
    audit(
        pool,
        user.company_id,
        Some(user.id),
        "port_updated",
        id,
        &json!({ "sections": sections }),
    )
    .await?;
    Ok(Json(detail(&row, order.as_ref(), None)))
}

/// Delete a draft port
///
/// Deletes a draft order in Telnyx (one Telnyx deleted already is fine)
/// and marks ours `deleted`; the numbers stay with the current carrier.
/// Only drafts (409 otherwise: cancel a submitted port with `POST
/// .../cancel`). Returns the order. Audited. Company admins (and super
/// admins, superusers) only.
#[utoipa::path(
    delete,
    path = "/v1/phone/admin/ports/{id}",
    operation_id = "delete_phone_port_order",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The order, deleted", body = PortOrderDetail),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "Not a draft", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortOrderDetail>> {
    let row = company_port(&state, user.company_id, id).await?;
    if row.status != PortStatus::Draft {
        return Err(ApiError::Conflict(
            "Only a draft can be deleted. Cancel the port instead.".into(),
        ));
    }
    state
        .telnyx
        .delete_porting_order(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "delete porting order"))?;
    port::mark_deleted(&state, &row, Some(user.id)).await?;
    audit(
        &state.pool,
        user.company_id,
        Some(user.id),
        "port_deleted",
        id,
        &json!({}),
    )
    .await?;
    let row = company_port(&state, user.company_id, id).await?;
    Ok(Json(detail(&row, None, None)))
}

/// Submit a number port
///
/// Submits the order to the current carrier through Telnyx (a draft, or
/// again after an exception once it is fixed). Telnyx checks that the
/// business details, the signed LOA, the bill and the date are there and
/// refuses otherwise (400, its reason in the message). Returns the order.
/// Audited. Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports/{id}/submit",
    operation_id = "submit_phone_port_order",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The order, submitted", body = PortOrderDetail),
        (status = 400, description = "Telnyx refused (something is missing)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "Not a draft or an order in exception", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn submit_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortOrderDetail>> {
    let pool = &state.pool;
    let row = company_port(&state, user.company_id, id).await?;
    if !row.status.submittable() {
        return Err(ApiError::Conflict("This port is submitted already.".into()));
    }
    let order = state
        .telnyx
        .confirm_porting_order(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "submit porting order"))?;
    sqlx::query(
        "UPDATE phone_port_orders SET submitted_at = UTC_TIMESTAMP(), submitted_by_user_id = ?
          WHERE id = ?",
    )
    .bind(user.id)
    .bind(id)
    .execute(pool)
    .await?;
    record(
        pool,
        user.company_id,
        Some(id),
        "submitted",
        None,
        None,
        Some(user.id),
    )
    .await?;
    let row = company_port(&state, user.company_id, id).await?;
    let row = port::sync(&state, &row, &order, Some(user.id)).await?;
    audit(
        pool,
        user.company_id,
        Some(user.id),
        "port_submitted",
        id,
        &json!({}),
    )
    .await?;
    Ok(Json(detail(&row, Some(&order), None)))
}

/// Cancel a number port
///
/// Asks Telnyx to cancel a submitted order: it goes `cancel-pending` while
/// Telnyx's porting team handles it, then `cancelled` (the admins are told).
/// Not within 48 hours of the confirmed port date (400: Telnyx support must
/// cancel then); a draft is deleted instead (409). Returns the order.
/// Audited. Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports/{id}/cancel",
    operation_id = "cancel_phone_port_order",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The order, cancel asked", body = PortOrderDetail),
        (status = 400, description = "Within 48 hours of the port date, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "A draft, or a port that is done or cancelling", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn cancel_port_order(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortOrderDetail>> {
    let pool = &state.pool;
    let row = company_port(&state, user.company_id, id).await?;
    if !row.status.cancellable() {
        return Err(ApiError::Conflict(if row.status == PortStatus::Draft {
            "A draft is deleted, not cancelled.".into()
        } else {
            "This port cannot be cancelled now.".into()
        }));
    }
    if row.too_close_to_cancel(Utc::now()) {
        return Err(ApiError::BadRequest(format!(
            "The port date is less than 48 hours away: ask Telnyx support to cancel it{}.",
            row.support_key
                .as_deref()
                .map(|key| format!(" (support key {key})"))
                .unwrap_or_default()
        )));
    }
    let order = state
        .telnyx
        .cancel_porting_order(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "cancel porting order"))?;
    sqlx::query("UPDATE phone_port_orders SET cancel_requested_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    record(
        pool,
        user.company_id,
        Some(id),
        "cancel_requested",
        None,
        None,
        Some(user.id),
    )
    .await?;
    let row = company_port(&state, user.company_id, id).await?;
    let row = port::sync(&state, &row, &order, Some(user.id)).await?;
    audit(
        pool,
        user.company_id,
        Some(user.id),
        "port_cancel_requested",
        id,
        &json!({}),
    )
    .await?;
    Ok(Json(detail(&row, Some(&order), None)))
}

/// One requirement of a port.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortRequirement {
    /// Its id: send it with a value (`PATCH`, `requirements`) or a document
    /// (`POST .../documents`, kind `requirement`).
    pub requirement_type_id: String,
    /// E.g. "Latest Invoice"; null when Telnyx gave none.
    pub name: Option<String>,
    /// What it asks for; null when Telnyx gave none.
    pub description: Option<String>,
    /// An example of what satisfies it; null when none.
    pub example: Option<String>,
    /// `document` or `textual`; null when Telnyx did not say.
    pub field_type: Option<String>,
    /// The text value given so far (textual ones); null otherwise.
    pub value: Option<String>,
    /// Something was given for it.
    pub fulfilled: bool,
    /// `requirement-info-pending` (to do), `requirement-info-under-review`,
    /// `requirement-info-exception` (redo) or `approved`; null when none.
    pub status: Option<String>,
    /// The values Telnyx accepts, when it lists them.
    pub acceptable_values: Vec<String>,
}

/// `GET /v1/phone/admin/ports/{id}/requirements`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortRequirements {
    /// In Telnyx's order.
    pub requirements: Vec<PortRequirement>,
}

/// List what a number port still needs
///
/// The order's requirements as Telnyx lists them now (by country and
/// number type): what each asks for, whether it was given and its review
/// status. Fulfil a textual one with `PATCH` (`requirements`), a document
/// with `POST .../documents` (kind `requirement`). 404 when it is not the
/// company's. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}/requirements",
    operation_id = "list_phone_port_requirements",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The order's requirements", body = PortRequirements),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_port_requirements(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortRequirements>> {
    let row = company_port(&state, user.company_id, id).await?;
    let listed = state
        .telnyx
        .porting_requirements(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "list porting requirements"))?;
    Ok(Json(PortRequirements {
        requirements: listed
            .into_iter()
            .map(|requirement| {
                let textual = requirement.field_type.as_deref() == Some("textual");
                PortRequirement {
                    fulfilled: requirement.field_value.is_some(),
                    value: requirement.field_value.filter(|_| textual),
                    requirement_type_id: requirement.requirement_type_id,
                    name: requirement.name,
                    description: requirement.description,
                    example: requirement.example,
                    field_type: requirement.field_type,
                    status: requirement.status,
                    acceptable_values: requirement.acceptable_values,
                }
            })
            .collect(),
    }))
}

/// A range of times a port date may be asked for.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortFocWindow {
    /// Its start (UTC).
    #[serde(serialize_with = "js_date_required")]
    pub start_at: DateTime<Utc>,
    /// Its end (UTC).
    #[serde(serialize_with = "js_date_required")]
    pub end_at: DateTime<Utc>,
}

/// `GET /v1/phone/admin/ports/{id}/foc-windows`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortFocWindows {
    /// Earliest first.
    pub windows: Vec<PortFocWindow>,
}

/// List the port dates Telnyx offers
///
/// The windows a port date may be asked for in (Telnyx's, by the current
/// carrier and the numbers; FastPort orders get earlier ones), earliest
/// first. Ask for one with `PATCH` (`focDatetimeRequested`); the current
/// carrier confirms the actual date later (`focAt`). 404 when it is not the
/// company's. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}/foc-windows",
    operation_id = "list_phone_port_foc_windows",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The windows", body = PortFocWindows),
        (status = 400, description = "Telnyx refused (the order is not ready for a date)", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_port_foc_windows(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortFocWindows>> {
    let row = company_port(&state, user.company_id, id).await?;
    let windows: Vec<FocWindow> = state
        .telnyx
        .allowed_foc_windows(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "list FOC windows"))?;
    Ok(Json(PortFocWindows {
        windows: windows
            .into_iter()
            .map(|window| PortFocWindow {
                start_at: window.started_at,
                end_at: window.ended_at,
            })
            .collect(),
    }))
}

/// Download a port's Letter of Authorization
///
/// The order's LOA from Telnyx, a PDF filled in from the order (save the
/// business details first): print or e-sign it, then upload it with `POST
/// .../documents` (kind `loa`). 404 when it is not the company's. Company
/// admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}/loa-template",
    operation_id = "download_phone_port_loa_template",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The LOA to sign", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn download_port_loa_template(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let row = company_port(&state, user.company_id, id).await?;
    let pdf = state
        .telnyx
        .porting_loa_template(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "download LOA template"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/pdf".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"letter-of-authorization-{id}.pdf\""),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        pdf,
    )
        .into_response())
}

wire_enum! {
    /// Which document is uploaded.
    pub enum PortDocumentKind {
        /// The signed Letter of Authorization.
        Loa = "loa",
        /// A recent bill of the current carrier (within 30 days is best).
        Invoice = "invoice",
        /// A document one of the order's requirements asks for
        /// (`requirementTypeId`).
        Requirement = "requirement",
    }
}

/// Body of `POST /v1/phone/admin/ports/{id}/documents`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortDocumentInput {
    /// `loa`, `invoice` or `requirement`.
    pub kind: PortDocumentKind,
    /// For `requirement`: the requirement it fulfils
    /// (`PortRequirement.requirementTypeId`).
    #[serde(default)]
    pub requirement_type_id: Option<String>,
    /// The file's name, e.g. `bill.pdf`.
    pub filename: String,
    /// The file, base64: a PDF, JPEG or PNG of at most 4 MB.
    pub content_base64: String,
}

/// The file's type by its first bytes, and the extension it should have.
fn document_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"%PDF-") {
        Some("pdf")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else {
        None
    }
}

/// A safe file name for Telnyx: no path, no control characters, at most
/// 100 characters, ending in the file's type.
fn document_name(typed: &str, extension: &str) -> String {
    let base = typed.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(LABEL_CHARS)
        .collect();
    let cleaned = cleaned.trim();
    let lower = cleaned.to_lowercase();
    let matches = match extension {
        "jpg" => lower.ends_with(".jpg") || lower.ends_with(".jpeg"),
        other => lower.ends_with(&format!(".{other}")),
    };
    if cleaned.is_empty() {
        format!("document.{extension}")
    } else if matches {
        cleaned.to_string()
    } else {
        format!("{cleaned}.{extension}")
    }
}

/// Attach a document to a number port
///
/// Uploads the file to Telnyx (a PDF, JPEG or PNG of at most 4 MB, sent
/// base64) and attaches it to the order: the signed LOA (`loa`), a recent
/// bill of the current carrier (`invoice`), or what a requirement asks for
/// (`requirement` with `requirementTypeId`). A new one replaces the one
/// attached. We keep no copy. Only while the order takes changes (409
/// otherwise). Returns the order with Telnyx's details. Audited (the kind
/// only). Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports/{id}/documents",
    operation_id = "upload_phone_port_document",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    request_body = PortDocumentInput,
    responses(
        (status = 200, description = "The order with the document attached", body = PortOrderDetail),
        (status = 400, description = "Not base64, too big, not a PDF, JPEG or PNG, no requirement named, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "The order can no longer be changed", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn upload_port_document(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<PortDocumentInput>,
) -> ApiResult<Json<PortOrderDetail>> {
    let pool = &state.pool;
    let row = company_port(&state, user.company_id, id).await?;
    if !row.status.editable() {
        return Err(ApiError::Conflict(
            "This port can no longer be changed.".into(),
        ));
    }
    let bytes = STANDARD
        .decode(input.content_base64.trim())
        .map_err(|_| ApiError::BadRequest("The file did not arrive whole. Try again.".into()))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("The file is empty.".into()));
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(ApiError::BadRequest(
            "The file is too big: upload a PDF, JPEG or PNG of at most 4 MB.".into(),
        ));
    }
    let extension = document_type(&bytes)
        .ok_or_else(|| ApiError::BadRequest("Upload a PDF, JPEG or PNG.".into()))?;
    let requirement = match input.kind {
        PortDocumentKind::Requirement => Some(
            input
                .requirement_type_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty() && id.len() <= 64)
                .ok_or_else(|| ApiError::BadRequest("Say which requirement it is for.".into()))?
                .to_string(),
        ),
        _ => None,
    };
    let config = port_setup(&state, user.company_id).await?;
    let filename = document_name(&input.filename, extension);
    let reference = format!("granite-c{}-port-{id}", user.company_id);
    let document = state
        .telnyx
        .upload_document(&filename, &bytes, &reference)
        .await
        .map_err(|err| telnyx_refused(&err, "upload porting document"))?;
    let changes = match (input.kind, requirement) {
        (PortDocumentKind::Loa, _) => json!({ "documents": { "loa": document } }),
        (PortDocumentKind::Invoice, _) => json!({ "documents": { "invoice": document } }),
        (PortDocumentKind::Requirement, Some(requirement)) => json!({
            "requirements": [{ "requirement_type_id": requirement, "field_value": document }]
        }),
        (PortDocumentKind::Requirement, None) => json!({}),
    };
    let (row, order) = send_changes(
        &state,
        &row,
        &config,
        changes,
        user.id,
        "attach porting document",
    )
    .await?;
    record(
        pool,
        user.company_id,
        Some(id),
        "document",
        None,
        Some(input.kind.as_str()),
        Some(user.id),
    )
    .await?;
    audit(
        pool,
        user.company_id,
        Some(user.id),
        "port_document_uploaded",
        id,
        &json!({ "kind": input.kind }),
    )
    .await?;
    Ok(Json(detail(&row, Some(&order), None)))
}

/// One entry of a port's timeline.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortTimelineEntry {
    /// `created`, `details` (sections saved), `document` (its kind),
    /// `submitted`, `status` (a status change), `foc` (the port date moved),
    /// `comment`, `split`, `cancel_requested`, `messaging` (texting's port),
    /// `completed` (the numbers added; `detail` = how many).
    pub kind: String,
    /// The status after a status change; null otherwise.
    pub status: Option<String>,
    /// A short fact: exception codes, the sections saved, the document
    /// kind, a count; null when none.
    pub detail: Option<String>,
    /// The admin who did it; null for Telnyx and our system.
    pub actor_name: Option<String>,
    /// When.
    #[serde(serialize_with = "js_date_required")]
    pub at: DateTime<Utc>,
}

/// `GET /v1/phone/admin/ports/{id}/timeline`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortTimeline {
    /// Oldest first.
    pub entries: Vec<PortTimelineEntry>,
}

/// List a port's timeline
///
/// What happened to the order, oldest first: who started, changed and
/// submitted it, every status change (with Telnyx's exception codes), port
/// date moves, comments from Telnyx, splits, cancellation, and the ported
/// numbers added. From our copy (no Telnyx call). 404 when it is not the
/// company's. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}/timeline",
    operation_id = "list_phone_port_timeline",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The timeline", body = PortTimeline),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_port_timeline(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortTimeline>> {
    company_port(&state, user.company_id, id).await?;
    let rows: Vec<TimelineRow> = port::timeline(&state.pool, user.company_id, id).await?;
    Ok(Json(PortTimeline {
        entries: rows
            .into_iter()
            .map(|row| PortTimelineEntry {
                kind: row.kind,
                status: row.status,
                detail: row.detail,
                actor_name: row.actor_name,
                at: row.occurred_at,
            })
            .collect(),
    }))
}

/// A comment on a port.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PortComment {
    /// Telnyx's id.
    pub id: String,
    /// The text.
    pub body: String,
    /// `telnyx` (Telnyx's porting team), `us` (written from here) or
    /// `system`.
    pub author: String,
    /// When; null when Telnyx did not say.
    #[serde(serialize_with = "js_date")]
    pub created_at: Option<DateTime<Utc>>,
}

/// `GET /v1/phone/admin/ports/{id}/comments`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PortComments {
    /// Oldest first.
    pub comments: Vec<PortComment>,
}

fn comment_of(comment: crate::telnyx::porting::PortingComment) -> PortComment {
    PortComment {
        author: match comment.user_type.as_str() {
            "admin" => "telnyx",
            "user" => "us",
            _ => "system",
        }
        .into(),
        id: comment.id,
        body: comment.body,
        created_at: comment.created_at,
    }
}

/// List a port's comments
///
/// The order's comments with Telnyx's porting team, oldest first, as
/// Telnyx has them now (they ask for documents or fixes here; the admins
/// are told when one comes). Reply with `POST .../comments`. 404 when it is
/// not the company's. Company admins (and super admins, superusers) only.
#[utoipa::path(
    get,
    path = "/v1/phone/admin/ports/{id}/comments",
    operation_id = "list_phone_port_comments",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    responses(
        (status = 200, description = "The comments", body = PortComments),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list_port_comments(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<PortComments>> {
    let row = company_port(&state, user.company_id, id).await?;
    let comments = state
        .telnyx
        .porting_comments(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "list porting comments"))?;
    Ok(Json(PortComments {
        comments: comments.into_iter().map(comment_of).collect(),
    }))
}

/// Body of `POST /v1/phone/admin/ports/{id}/comments`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PortCommentInput {
    /// The text for Telnyx's porting team, up to 2000 characters.
    pub body: String,
}

/// Write to Telnyx's porting team on a port
///
/// Adds a comment to the order (answer their questions, say a fix is
/// done). Returns it. Not on a deleted draft (409). Audited (not the text).
/// Company admins (and super admins, superusers) only.
#[utoipa::path(
    post,
    path = "/v1/phone/admin/ports/{id}/comments",
    operation_id = "add_phone_port_comment",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    request_body = PortCommentInput,
    responses(
        (status = 201, description = "The comment", body = PortComment),
        (status = 400, description = "Empty or too long, or Telnyx refused", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "The draft was deleted", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn add_port_comment(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<PortCommentInput>,
) -> ApiResult<(StatusCode, Json<PortComment>)> {
    let row = company_port(&state, user.company_id, id).await?;
    if row.status == PortStatus::Deleted {
        return Err(ApiError::Conflict("This draft was deleted.".into()));
    }
    let body = required(&input.body, "comment", COMMENT_CHARS)?;
    let comment = state
        .telnyx
        .create_porting_comment(&row.telnyx_order_id, &body)
        .await
        .map_err(|err| telnyx_refused(&err, "add porting comment"))?;
    record(
        &state.pool,
        user.company_id,
        Some(id),
        "comment",
        None,
        Some("us"),
        Some(user.id),
    )
    .await?;
    audit(
        &state.pool,
        user.company_id,
        Some(user.id),
        "port_commented",
        id,
        &json!({}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(comment_of(comment))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_known_by_their_bytes_and_named_safely() {
        assert_eq!(document_type(b"%PDF-1.7 ..."), Some("pdf"));
        assert_eq!(document_type(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(document_type(&[0x89, b'P', b'N', b'G', 0x0D]), Some("png"));
        assert_eq!(document_type(b"GIF89a"), None);
        assert_eq!(
            document_name("C:\\bills\\October bill.PDF", "pdf"),
            "October bill.PDF"
        );
        assert_eq!(document_name("../../etc/passwd", "pdf"), "passwd.pdf");
        assert_eq!(document_name("scan.jpeg", "jpg"), "scan.jpeg");
        assert_eq!(document_name("  ", "png"), "document.png");
        assert_eq!(document_name("a\"b\u{7}.png", "png"), "ab.png");
    }

    #[test]
    fn changes_merge_into_the_setup() {
        let mut body =
            json!({ "webhook_url": "h", "phone_number_configuration": { "connection_id": "app" } });
        merge(
            &mut body,
            json!({ "end_user": { "admin": { "entity_name": "Acme" } },
                    "phone_number_configuration": { "tags": ["x"] } }),
        );
        assert_eq!(
            body,
            json!({ "webhook_url": "h",
                    "phone_number_configuration": { "connection_id": "app", "tags": ["x"] },
                    "end_user": { "admin": { "entity_name": "Acme" } } })
        );
    }

    #[test]
    fn numbers_are_checked_without_naming_them() {
        assert_eq!(
            checked_numbers(&[
                "317-555-0101".into(),
                "+13175550101".into(),
                "(317) 555 0102".into()
            ])
            .unwrap(),
            ["+13175550101", "+13175550102"]
        );
        let err = checked_numbers(&["+442071838750".into()]).unwrap_err();
        assert!(!err.to_string().contains("4420"));
        assert!(checked_numbers(&[]).is_err());
        let many: Vec<String> = (0..51).map(|n| format!("317555{n:04}")).collect();
        assert!(checked_numbers(&many).is_err());
    }
}
