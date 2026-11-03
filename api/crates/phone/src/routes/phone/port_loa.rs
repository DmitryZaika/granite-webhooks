//! E-signing a port's Letter of Authorization (a port's Documents step):
//! the authorized person signs on screen and the signed LOA is attached to
//! the order, with nothing to download or upload.
//!
//! - `POST /v1/phone/admin/ports/{id}/loa/sign`   → the admin is the authorized person and signs now
//! - `POST /v1/phone/admin/ports/{id}/loa/invite` → email the authorized person a one-time link
//! - `GET  /v1/public/port-loa/{token}`           → what the link's page shows
//! - `GET  /v1/public/port-loa/{token}/template`  → the LOA to sign (a PDF)
//! - `POST /v1/public/port-loa/{token}/sign`      → sign through the link
//!
//! The page finds where to sign on Telnyx's LOA and sends those boxes with
//! the drawn signature; we read the LOA from Telnyx again and print the
//! signature and today's date (the company's time zone) on it ourselves, so
//! only the signature comes from the browser. A link is good for 7 days
//! and one signature; only its SHA-256 is kept.

use super::porting::{
    PortDocumentKind, PortOrderDetail, attach_document, company_port, telnyx_refused,
};
use crate::app_origin::{app_origin, company_subdomain};
use crate::auth::{AdminUser, sha256_hex};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::phone::company;
use crate::phone::porting::{self as port, PortRow, audit, record};
use crate::signing_pdf::{
    FieldKind, SIGNATURE_IMAGE_PREFIX, SignatureField, prepare_for_signwell, validate_values,
};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// How long a link works.
const INVITE_DAYS: i64 = 7;
/// Most boxes the page may send (signatures and dates).
const MAX_BOXES: usize = 8;
/// The largest drawn signature, as a data URL.
const MAX_SIGNATURE_CHARS: usize = 700_000;
const NAME_CHARS: usize = 100;
const NO_REPLY: &str = "noreply@granite-manager.com";
const SIGNED_FILENAME: &str = "letter-of-authorization-signed.pdf";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(sign_port_loa))
        .routes(routes!(invite_port_loa_signer))
        .routes(routes!(view_port_loa_invite))
        .routes(routes!(port_loa_invite_template))
        .routes(routes!(sign_port_loa_invite))
}

/// Where the signature or the date goes on the LOA: points from the page's
/// top-left corner, as the signing editor finds them.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LoaBox {
    /// `signature` or `date`.
    #[serde(rename = "type")]
    pub kind: LoaBoxKind,
    /// From 1.
    pub page: i64,
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// What a box holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoaBoxKind {
    Signature,
    Date,
}

/// Body of `POST /v1/phone/admin/ports/{id}/loa/sign`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoaSignInput {
    /// Where to print the signature and the date.
    pub fields: Vec<LoaBox>,
    /// The drawn signature: a PNG data URL.
    pub signature: String,
    /// The authorized person the admin confirms they are.
    pub signer_name: String,
}

/// Body of `POST /v1/phone/admin/ports/{id}/loa/invite`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoaInviteInput {
    /// The authorized person.
    pub signer_name: String,
    /// Where the link goes.
    pub email: String,
}

/// The link was emailed.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoaInviteSent {
    /// Who it went to.
    pub email: String,
    /// When the link stops working.
    pub expires_at: DateTime<Utc>,
}

/// Body of `POST /v1/public/port-loa/{token}/sign`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoaLinkSignInput {
    /// Where to print the signature and the date.
    pub fields: Vec<LoaBox>,
    /// The drawn signature: a PNG data URL.
    pub signature: String,
}

/// Where a link is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LoaInviteStatus {
    /// Waiting for the signature.
    Open,
    /// Signed through this link.
    Signed,
    /// Past 7 days, replaced by a newer link, or the port can no longer change.
    Expired,
}

/// What the link's page shows.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoaInviteView {
    /// Where the link is.
    pub status: LoaInviteStatus,
    /// The authorized person asked to sign.
    pub signer_name: String,
    /// The company moving its numbers.
    pub company_name: String,
    /// The numbers moving (E.164).
    pub numbers: Vec<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct InviteRow {
    id: i64,
    company_id: i32,
    port_order_id: i64,
    signer_name: String,
    created_by_user_id: i32,
    expires_at: DateTime<Utc>,
    signed_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

/// The typed name: trimmed, 1 to 100 characters.
fn checked_name(typed: &str) -> ApiResult<String> {
    let name = typed.trim();
    if name.is_empty() || name.chars().count() > NAME_CHARS {
        return Err(ApiError::BadRequest(
            "Type the authorized person's name (at most 100 characters).".into(),
        ));
    }
    Ok(name.to_string())
}

/// The typed email, trimmed: one `@`, a dot after it, no spaces.
fn checked_email(typed: &str) -> ApiResult<String> {
    let email = typed.trim();
    let valid = email.len() <= 255
        && !email.chars().any(char::is_whitespace)
        && email.split_once('@').is_some_and(|(user, domain)| {
            !user.is_empty() && !domain.contains('@') && domain.contains('.')
        });
    if !valid {
        return Err(ApiError::BadRequest("Enter a valid email address.".into()));
    }
    Ok(email.to_string())
}

/// The boxes as fields with what we print in them: the signature, and
/// `date` in the date boxes.
fn stamp_fields(boxes: &[LoaBox], signature: &str, date: &str) -> ApiResult<Vec<SignatureField>> {
    if !boxes.iter().any(|b| b.kind == LoaBoxKind::Signature) || boxes.len() > MAX_BOXES {
        return Err(ApiError::BadRequest(
            "Could not find where to sign on this LOA. Download it and upload a signed copy instead."
                .into(),
        ));
    }
    let signature = signature.trim();
    if !signature.starts_with(SIGNATURE_IMAGE_PREFIX) || signature.len() > MAX_SIGNATURE_CHARS {
        return Err(ApiError::BadRequest(
            "Draw your signature, then try again.".into(),
        ));
    }
    let fields: Vec<SignatureField> = boxes
        .iter()
        .map(|b| SignatureField {
            kind: match b.kind {
                LoaBoxKind::Signature => FieldKind::Signature,
                LoaBoxKind::Date => FieldKind::Date,
            },
            page: b.page,
            x: b.x,
            y: b.y,
            width: b.width,
            height: b.height,
            required: true,
            value: Some(match b.kind {
                LoaBoxKind::Signature => signature.to_string(),
                LoaBoxKind::Date => date.to_string(),
            }),
            prefill: None,
            label: None,
            id: None,
        })
        .collect();
    for field in &fields {
        field
            .check()
            .map_err(|reason| ApiError::BadRequest(format!("A signing box is off: {reason}.")))?;
    }
    validate_values(&fields).map_err(|err| ApiError::BadRequest(err.message))?;
    Ok(fields)
}

/// Today in the company's time zone, as US forms write it: `11/03/2026`.
async fn today(state: &AppState, company_id: i32) -> ApiResult<String> {
    let config = company::load(&state.pool, company_id).await?;
    let zone = company::time_zone_of(config.as_ref());
    Ok(Utc::now()
        .with_timezone(&zone)
        .format("%m/%d/%Y")
        .to_string())
}

/// Reads the LOA from Telnyx, prints the signature and today's date in the
/// boxes and attaches it to the order as `actor`'s; audited with who signed
/// and how (`in_app` or `link`).
async fn sign_and_attach(
    state: &AppState,
    row: &PortRow,
    boxes: &[LoaBox],
    signature: &str,
    signer: &str,
    how: &str,
    actor: i32,
) -> ApiResult<PortOrderDetail> {
    if !row.status.editable() {
        return Err(ApiError::Conflict(
            "This port can no longer be changed.".into(),
        ));
    }
    let date = today(state, row.company_id).await?;
    let fields = stamp_fields(boxes, signature, &date)?;
    let template = state
        .telnyx
        .porting_loa_template(&row.telnyx_order_id)
        .await
        .map_err(|err| telnyx_refused(&err, "download LOA template"))?;
    let signed = prepare_for_signwell(&template, &fields).map_err(|_| {
        ApiError::BadRequest(
            "Could not put the signature on this LOA. Download it and upload a signed copy instead."
                .into(),
        )
    })?;
    if !signed.stamped_text {
        return Err(ApiError::BadRequest(
            "Could not put the signature on this LOA. Download it and upload a signed copy instead."
                .into(),
        ));
    }
    let detail = attach_document(
        state,
        row,
        PortDocumentKind::Loa,
        None,
        SIGNED_FILENAME,
        &signed.bytes,
        actor,
    )
    .await?;
    audit(
        &state.pool,
        row.company_id,
        Some(actor),
        "port_loa_signed",
        row.id,
        &json!({ "signer": signer, "how": how, "date": date }),
    )
    .await?;
    Ok(detail)
}

/// Sign a port's LOA now
///
/// For the authorized person signing on this screen: prints the drawn
/// signature and today's date in the boxes on Telnyx's LOA and attaches it
/// as the signed LOA (it replaces one attached). Only while the order takes
/// changes (409 otherwise). Audited with the confirmed name. Company admins
/// (and super admins, superusers) only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/admin/ports/{id}/loa/sign",
    operation_id = "sign_phone_port_loa",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    request_body = LoaSignInput,
    responses(
        (status = 200, description = "The order with the signed LOA attached", body = PortOrderDetail),
        (status = 400, description = "No name, no signature box, no drawn signature, or Telnyx refused", body = ErrorBody),
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
pub async fn sign_port_loa(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<LoaSignInput>,
) -> ApiResult<Json<PortOrderDetail>> {
    let signer = checked_name(&input.signer_name)?;
    let row = company_port(&state, user.company_id, id).await?;
    sign_and_attach(
        &state,
        &row,
        &input.fields,
        &input.signature,
        &signer,
        "in_app",
        user.id,
    )
    .await
    .map(Json)
}

fn html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn company_name(state: &AppState, company_id: i32) -> ApiResult<String> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(company_id)
        .fetch_optional(&state.pool)
        .await?;
    Ok(name.unwrap_or_default())
}

/// The numbers as people read them: `317-676-7938, 317-555-0100`.
fn numbers_text(numbers: &[String]) -> String {
    numbers
        .iter()
        .map(|number| {
            let digits: String = number.chars().filter(char::is_ascii_digit).collect();
            let ten = digits
                .strip_prefix('1')
                .filter(|d| d.len() == 10)
                .unwrap_or(digits.as_str());
            if ten.len() == 10 {
                format!("{}-{}-{}", &ten[..3], &ten[3..6], &ten[6..])
            } else {
                number.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Send the LOA to be signed
///
/// Emails the authorized person a link to sign the port's LOA on a page of
/// its own (no account; good for 7 days and one signature). A new link
/// replaces the port's open one. The order must take changes (409
/// otherwise). Audited. Company admins (and super admins, superusers) only.
#[crate::openapi::path(
    post,
    path = "/v1/phone/admin/ports/{id}/loa/invite",
    operation_id = "invite_phone_port_loa_signer",
    tag = "phone",
    params(("id" = i64, Path, description = "The port order (`PortOrder.id`)")),
    request_body = LoaInviteInput,
    responses(
        (status = 200, description = "The link was emailed", body = LoaInviteSent),
        (status = 400, description = "No name or not an email address", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 404, description = "No such port in the caller's company", body = ErrorBody),
        (status = 409, description = "The order can no longer be changed", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "The email could not be sent; try again", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn invite_port_loa_signer(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<LoaInviteInput>,
) -> ApiResult<Json<LoaInviteSent>> {
    let signer = checked_name(&input.signer_name)?;
    let email = checked_email(&input.email)?;
    let row = company_port(&state, user.company_id, id).await?;
    if !row.status.editable() {
        return Err(ApiError::Conflict(
            "This port can no longer be changed.".into(),
        ));
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let expires_at = Utc::now() + Duration::days(INVITE_DAYS);
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE phone_port_loa_invites SET revoked_at = UTC_TIMESTAMP(3)
          WHERE company_id = ? AND port_order_id = ? AND signed_at IS NULL AND revoked_at IS NULL",
    )
    .bind(row.company_id)
    .bind(row.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO phone_port_loa_invites
           (company_id, port_order_id, token_hash, signer_name, signer_email,
            created_by_user_id, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.company_id)
    .bind(row.id)
    .bind(sha256_hex(&token))
    .bind(&signer)
    .bind(&email)
    .bind(user.id)
    .bind(expires_at.naive_utc())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let subdomain = company_subdomain(&state.pool, row.company_id).await?;
    let link = format!(
        "{}/sign/port-loa/{token}",
        app_origin(&state.app_url, subdomain.as_deref())
    );
    let company = company_name(&state, row.company_id).await?;
    let sender = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(user.email.as_str())
        .to_string();
    let numbers = numbers_text(&row.numbers());
    let outgoing = OutgoingEmail {
        from_name: Some("Granite Phone".into()),
        from_address: NO_REPLY.into(),
        to: vec![email.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("Please sign the Letter of Authorization for {company}"),
        body: format!(
            "<p>Hi {},</p><p>{} asked you to sign the Letter of Authorization that moves \
             {} for {} to Telnyx. It takes a minute: read it, draw your signature and \
             click Sign.</p><p><a href=\"{}\">Review and sign</a></p><p>The link works \
             for {INVITE_DAYS} days.</p>",
            html(&signer),
            html(&sender),
            html(&numbers),
            html(&company),
            html(&link),
        ),
        signature: None,
        reply_to: vec![user.email.clone()],
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    state.webhooks.send_email(&outgoing).await.map_err(|err| {
        tracing::warn!(?err, order_id = row.id, "LOA signing link not sent");
        ApiError::Upstream("The email could not be sent. Try again.".into())
    })?;
    record(
        &state.pool,
        row.company_id,
        Some(row.id),
        "loa_sent",
        None,
        Some(&signer),
        Some(user.id),
    )
    .await?;
    audit(
        &state.pool,
        row.company_id,
        Some(user.id),
        "port_loa_link_sent",
        row.id,
        &json!({ "signer": signer, "email": email }),
    )
    .await?;
    Ok(Json(LoaInviteSent { email, expires_at }))
}

/// The link's invite and port: 404 for an unknown token.
async fn invite_of(state: &AppState, token: &str) -> ApiResult<(InviteRow, PortRow)> {
    if token.is_empty() || token.len() > 100 {
        return Err(ApiError::NotFound);
    }
    let invite: InviteRow = sqlx::query_as(
        "SELECT id, company_id, port_order_id, signer_name, created_by_user_id,
                expires_at, signed_at, revoked_at
           FROM phone_port_loa_invites WHERE token_hash = ?",
    )
    .bind(sha256_hex(token))
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let row = port::load(&state.pool, invite.company_id, invite.port_order_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((invite, row))
}

fn status_of(invite: &InviteRow, row: &PortRow) -> LoaInviteStatus {
    if invite.signed_at.is_some() {
        LoaInviteStatus::Signed
    } else if invite.revoked_at.is_some()
        || invite.expires_at <= Utc::now()
        || !row.status.editable()
    {
        LoaInviteStatus::Expired
    } else {
        LoaInviteStatus::Open
    }
}

async fn view(state: &AppState, invite: &InviteRow, row: &PortRow) -> ApiResult<LoaInviteView> {
    Ok(LoaInviteView {
        status: status_of(invite, row),
        signer_name: invite.signer_name.clone(),
        company_name: company_name(state, row.company_id).await?,
        numbers: row.numbers(),
    })
}

/// A signing link's page
///
/// Who is asked to sign which port, and whether the link still works. No
/// session: the link's token is the proof. 404 for an unknown link.
#[crate::openapi::path(
    get,
    path = "/v1/public/port-loa/{token}",
    operation_id = "view_port_loa_invite",
    tag = "phone",
    params(("token" = String, Path, description = "The link's token")),
    responses(
        (status = 200, description = "The page", body = LoaInviteView),
        (status = 404, description = "No such link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn view_port_loa_invite(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<LoaInviteView>> {
    let (invite, row) = invite_of(&state, &token).await?;
    view(&state, &invite, &row).await.map(Json)
}

/// A signing link's LOA
///
/// The port's LOA from Telnyx (a PDF), while the link works (409 once it
/// was signed or expired). No session: the link's token is the proof.
#[crate::openapi::path(
    get,
    path = "/v1/public/port-loa/{token}/template",
    operation_id = "port_loa_invite_template",
    tag = "phone",
    params(("token" = String, Path, description = "The link's token")),
    responses(
        (status = 200, description = "The LOA to sign", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Telnyx refused", body = ErrorBody),
        (status = 404, description = "No such link", body = ErrorBody),
        (status = 409, description = "The link was signed or expired", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
    )
)]
pub async fn port_loa_invite_template(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let (invite, row) = invite_of(&state, &token).await?;
    if status_of(&invite, &row) != LoaInviteStatus::Open {
        return Err(ApiError::Conflict("This link no longer works.".into()));
    }
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
                "inline; filename=\"letter-of-authorization.pdf\"".to_string(),
            ),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        pdf,
    )
        .into_response())
}

/// Sign through a link
///
/// Prints the drawn signature and today's date in the boxes on the port's
/// LOA and attaches it as the signed LOA, as the admin who sent the link;
/// the link then stops working and that admin is emailed. 409 once the link
/// was signed or expired. No session: the link's token is the proof.
#[crate::openapi::path(
    post,
    path = "/v1/public/port-loa/{token}/sign",
    operation_id = "sign_port_loa_invite",
    tag = "phone",
    params(("token" = String, Path, description = "The link's token")),
    request_body = LoaLinkSignInput,
    responses(
        (status = 200, description = "Signed", body = LoaInviteView),
        (status = 400, description = "No signature box, no drawn signature, or Telnyx refused", body = ErrorBody),
        (status = 404, description = "No such link", body = ErrorBody),
        (status = 409, description = "The link was signed or expired", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
    )
)]
pub async fn sign_port_loa_invite(
    State(state): State<AppState>,
    Path(token): Path<String>,
    JsonBody(input): JsonBody<LoaLinkSignInput>,
) -> ApiResult<Json<LoaInviteView>> {
    let (invite, row) = invite_of(&state, &token).await?;
    if status_of(&invite, &row) != LoaInviteStatus::Open {
        return Err(ApiError::Conflict("This link no longer works.".into()));
    }
    sign_and_attach(
        &state,
        &row,
        &input.fields,
        &input.signature,
        &invite.signer_name,
        "link",
        invite.created_by_user_id,
    )
    .await?;
    sqlx::query(
        "UPDATE phone_port_loa_invites SET signed_at = UTC_TIMESTAMP(3)
          WHERE id = ? AND signed_at IS NULL",
    )
    .bind(invite.id)
    .execute(&state.pool)
    .await?;
    notify_sender(&state, &invite, &row).await;
    let (invite, row) = invite_of(&state, &token).await?;
    view(&state, &invite, &row).await.map(Json)
}

/// Emails the admin who sent the link that it was signed (a failure is only
/// logged: the LOA is attached either way).
async fn notify_sender(state: &AppState, invite: &InviteRow, row: &PortRow) {
    let email: Option<String> = sqlx::query_scalar(
        "SELECT email FROM users WHERE id = ? AND is_deleted = 0 AND email IS NOT NULL AND email <> ''",
    )
    .bind(invite.created_by_user_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(email) = email else {
        return;
    };
    let subdomain = company_subdomain(&state.pool, row.company_id)
        .await
        .ok()
        .flatten();
    let link = format!(
        "{}/admin/phone/ports/{}",
        app_origin(&state.app_url, subdomain.as_deref()),
        row.id
    );
    let outgoing = OutgoingEmail {
        from_name: Some("Granite Phone".into()),
        from_address: NO_REPLY.into(),
        to: vec![email],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("[Granite Phone] {} signed the LOA", invite.signer_name),
        body: format!(
            "<p>{} signed the Letter of Authorization for {}. It is attached to the port.</p>\
             <p><a href=\"{}\">Open the port</a></p>",
            html(&invite.signer_name),
            html(&numbers_text(&row.numbers())),
            html(&link)
        ),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(err) = state.webhooks.send_email(&outgoing).await {
        tracing::warn!(?err, order_id = row.id, "LOA signed email not sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature_box() -> LoaBox {
        LoaBox {
            kind: LoaBoxKind::Signature,
            page: 1,
            x: 72.0,
            y: 600.0,
            width: 200.0,
            height: 40.0,
        }
    }

    #[test]
    fn names_and_emails_are_checked() {
        assert_eq!(checked_name("  Georgy Galkin ").unwrap(), "Georgy Galkin");
        assert!(checked_name("   ").is_err());
        assert_eq!(checked_email(" a@b.co ").unwrap(), "a@b.co");
        assert!(checked_email("a@b").is_err());
        assert!(checked_email("a b@c.co").is_err());
        assert!(checked_email("@c.co").is_err());
    }

    #[test]
    fn the_signature_and_the_date_are_printed_in_their_boxes() {
        let date_box = LoaBox {
            kind: LoaBoxKind::Date,
            ..signature_box()
        };
        // A 1×1 PNG.
        let png = format!(
            "{SIGNATURE_IMAGE_PREFIX}iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
        );
        let fields = stamp_fields(&[signature_box(), date_box], &png, "11/03/2026").unwrap();
        assert_eq!(fields[0].kind, FieldKind::Signature);
        assert_eq!(fields[0].value.as_deref(), Some(png.as_str()));
        assert_eq!(fields[1].kind, FieldKind::Date);
        assert_eq!(fields[1].value.as_deref(), Some("11/03/2026"));
        // No signature box, or no drawn signature: nothing is printed.
        let date_only = LoaBox {
            kind: LoaBoxKind::Date,
            ..signature_box()
        };
        assert!(stamp_fields(&[date_only], &png, "11/03/2026").is_err());
        assert!(stamp_fields(&[signature_box()], "not a picture", "11/03/2026").is_err());
    }

    #[test]
    fn numbers_read_like_phone_numbers() {
        assert_eq!(
            numbers_text(&["+13176767938".into(), "+14633292269".into()]),
            "317-676-7938, 463-329-2269"
        );
    }
}
