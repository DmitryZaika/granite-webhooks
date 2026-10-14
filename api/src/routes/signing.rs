//! The signer's side of e-signatures (no session: the invitation proves it).
//!
//! - `POST /v1/public/signing/{document_id}/session`  → trade the invitation token for a signing session
//! - `GET  /v1/public/signing/{document_id}`          → what to show: a native document, or `SignWell`'s page
//! - `POST /v1/public/signing/{document_id}/actions`  → start, finish, resume or decline
//! - `GET  /v1/public/signing/{document_id}/download?kind=` → the document to read, or the signed copy
//!
//! A native invitation link (`/sign/{id}?token=`) is traded once for a
//! 30-minute session in an `HttpOnly` `__Host-` cookie of this API's host;
//! every action also carries a CSRF token keyed from it
//! (`SIGNATURE_SESSION_SECRET`) and must come from `SIGNATURE_APP_ORIGIN`.
//! Finishing records an immutable claim first, then renders and stores the
//! signed PDF in the private signature bucket; a failure leaves a claim the
//! signer resumes. Port of `nativeSigning.server.ts` and
//! `nativeSigningController.server.ts`; `SignWell` documents get their
//! embedded signing link (the Remix `sign.$documentId` loader).

use crate::routes::signatures::MAX_INLINE_BYTES;
use crate::routes::signwell_webhook::notify_status;
use crate::signing_pdf::{
    self, FieldKind, PdfSigningError, SignatureField, SigningAudit, sha256_hex,
};
use crate::state::AppState;
use crate::webhooks_client::{OutgoingAttachment, OutgoingEmail};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{NaiveDateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sqlx::{MySql, Transaction};
use std::collections::{BTreeMap, HashSet};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(open_session))
        .routes(routes!(signing_view))
        .routes(routes!(signing_action))
        .routes(routes!(download))
}

pub const CONSENT_VERSION: &str = "native-v1";
pub const CONSENT_TEXT: &str = "I have reviewed this document and agree to sign it electronically.";
const SESSION_MINUTES: i64 = 30;
const LEASE_SECONDS: i64 = 120;
const MAX_ACTION_BYTES: usize = 6_100_000;

const NO_STORE: [(&str, &str); 7] = [
    ("cache-control", "private, no-store, max-age=0"),
    ("pragma", "no-cache"),
    ("expires", "0"),
    ("referrer-policy", "no-referrer"),
    ("x-robots-tag", "noindex, nofollow"),
    ("x-content-type-options", "nosniff"),
    ("vary", "Cookie"),
];

fn no_store(mut response: Response) -> Response {
    for (name, value) in NO_STORE {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Why a signing request was refused, for the signer (`NativeSigningError`).
#[derive(Debug, Clone)]
pub struct SignerError {
    pub code: String,
    pub status: StatusCode,
    pub message: String,
}

impl SignerError {
    fn new(code: &str, status: StatusCode, message: &str) -> Self {
        Self {
            code: code.to_string(),
            status,
            message: message.to_string(),
        }
    }
}

/// A refusal: `{"error", "code"}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignerErrorBody {
    /// For the signer.
    pub error: String,
    /// `invalid_link`, `expired`, `signed`, `declined`, `csrf`,
    /// `review_required`, `invalid_submission`, `processing`,
    /// `retryable_finalization`, `idempotency_conflict`, `no_claim`,
    /// `version_changed`, `decline_disabled`, `parity_pending`, `unavailable`.
    pub code: String,
}

impl IntoResponse for SignerError {
    fn into_response(self) -> Response {
        no_store(
            (
                self.status,
                Json(SignerErrorBody {
                    error: self.message,
                    code: self.code,
                }),
            )
                .into_response(),
        )
    }
}

impl From<sqlx::Error> for SignerError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(?error, "native signing database error");
        unavailable()
    }
}

impl From<PdfSigningError> for SignerError {
    fn from(_: PdfSigningError) -> Self {
        Self::new(
            "invalid_submission",
            StatusCode::BAD_REQUEST,
            "Check the required fields and consent, then try again.",
        )
    }
}

fn unavailable() -> SignerError {
    SignerError::new(
        "unavailable",
        StatusCode::SERVICE_UNAVAILABLE,
        "Signing is temporarily unavailable. Please retry.",
    )
}

fn invalid_link() -> SignerError {
    SignerError::new(
        "invalid_link",
        StatusCode::NOT_FOUND,
        "This signing link is not valid.",
    )
}

type SignerResult<T> = Result<T, SignerError>;

fn hash(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

fn now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok()
}

/// 32 random bytes, base64url without padding.
fn valid_token(value: &str) -> bool {
    value.len() == 43
        && URL_SAFE_NO_PAD
            .decode(value)
            .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(&bytes) == value)
}

fn cookie_name(document_id: &str) -> String {
    format!("__Host-native_signing_{document_id}")
}

/// The signer's session: the raw value of the document's cookie.
fn session(headers: &HeaderMap, document_id: &str) -> SignerResult<String> {
    if !valid_uuid(document_id) {
        return Err(invalid_link());
    }
    let name = cookie_name(document_id);
    let values: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|header| header.split(';'))
        .map(str::trim)
        .filter_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
        .collect();
    match values.as_slice() {
        [raw] if valid_token(raw) => Ok(raw.clone()),
        _ => Err(invalid_link()),
    }
}

fn csrf_for(secret: &str, raw_session: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).expect("any key");
    mac.update(raw_session.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn secret(state: &AppState) -> SignerResult<&str> {
    state
        .signing_session_secret
        .as_deref()
        .filter(|_| {
            let origin = &*state.signature_app_origin;
            // Plain HTTP only for a local signing page (development, E2E).
            origin.starts_with("https://")
                || origin.starts_with("http://localhost:")
                || origin.starts_with("http://127.0.0.1:")
        })
        .ok_or_else(unavailable)
}

/// A native document as the signer may see it (`NativeRequest`).
#[derive(Debug, Clone)]
struct Snapshot {
    request_id: i32,
    company_id: i32,
    document_id: String,
    status: String,
    signer_name: String,
    signer_email: String,
    filename: String,
    prepared_object_key: String,
    prepared_sha256: String,
    document_version: String,
    fields: Vec<SignatureField>,
    settings: Value,
    completed_object_key: Option<String>,
    completed_sha256: Option<String>,
    expires_at: Option<NaiveDateTime>,
}

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    request_id: i32,
    company_id: i32,
    document_id: String,
    status: String,
    signer_name: String,
    signer_email: String,
    filename: String,
    prepared_object_key: String,
    prepared_sha256: String,
    document_version: String,
    fields_json: String,
    settings_json: String,
    completed_object_key: Option<String>,
    completed_sha256: Option<String>,
    expires_at: Option<NaiveDateTime>,
    revoked_at: Option<NaiveDateTime>,
    #[sqlx(default)]
    session_expires_at: Option<NaiveDateTime>,
    #[sqlx(default)]
    reviewed_at: Option<NaiveDateTime>,
}

const SNAPSHOT_COLUMNS: &str =
    "nd.request_id, nd.company_id, nd.document_id, CAST(sr.status AS CHAR) AS status,
    sr.signer_name, sr.signer_email, sr.filename, nd.prepared_object_key, nd.prepared_sha256,
    nd.document_version, CAST(nd.fields_json AS CHAR) AS fields_json,
    CAST(nd.settings_json AS CHAR) AS settings_json, nd.completed_object_key, nd.completed_sha256,
    nd.expires_at, nd.revoked_at";

fn stored_fields_valid(fields: &[SignatureField]) -> bool {
    let ids: HashSet<&str> = fields.iter().filter_map(|f| f.id.as_deref()).collect();
    (1..=20).contains(&fields.len())
        && ids.len() == fields.len()
        && fields.iter().all(|f| {
            f.check().is_ok()
                && f.id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty() && id.len() <= 64)
        })
        && signing_pdf::has_signer_signature(fields)
}

/// `parseNativeRequestSnapshot`: `None` for a revoked, foreign or broken row.
fn parse_snapshot(row: &SnapshotRow) -> Option<Snapshot> {
    if row.revoked_at.is_some() {
        return None;
    }
    let fields: Vec<SignatureField> = serde_json::from_str(&row.fields_json).ok()?;
    let settings: Value = serde_json::from_str(&row.settings_json).ok()?;
    if !stored_fields_valid(&fields)
        || !crate::native_delivery::frozen_settings_valid(&settings)
        || row.prepared_sha256 != row.document_version
        || !row
            .prepared_object_key
            .starts_with(&format!("{}/signatures/", row.company_id))
    {
        return None;
    }
    Some(Snapshot {
        request_id: row.request_id,
        company_id: row.company_id,
        document_id: row.document_id.clone(),
        status: row.status.clone(),
        signer_name: row.signer_name.clone(),
        signer_email: row.signer_email.clone(),
        filename: row.filename.clone(),
        prepared_object_key: row.prepared_object_key.clone(),
        prepared_sha256: row.prepared_sha256.clone(),
        document_version: row.document_version.clone(),
        fields,
        settings,
        completed_object_key: row.completed_object_key.clone(),
        completed_sha256: row.completed_sha256.clone(),
        expires_at: row.expires_at,
    })
}

/// Only invitation links, no reassigning and no uploaded signatures yet.
fn supported(document: &Snapshot) -> SignerResult<()> {
    let methods = document.settings["signatureMethods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if document.settings["authentication"] != "invitation"
        || document.settings["allowReassign"] == Value::Bool(true)
        || methods.iter().any(|m| m == "upload")
    {
        return Err(SignerError::new(
            "parity_pending",
            StatusCode::SERVICE_UNAVAILABLE,
            "This signing configuration is not yet available.",
        ));
    }
    Ok(())
}

fn expired(document: &Snapshot) -> bool {
    document.status == "expired" || document.expires_at.is_some_and(|at| at <= now())
}

/// The document can still be signed or declined.
fn active(document: &Snapshot) -> SignerResult<()> {
    if expired(document) {
        return Err(SignerError::new(
            "expired",
            StatusCode::GONE,
            "This signing link has expired.",
        ));
    }
    if !matches!(document.status.as_str(), "sent" | "viewed") {
        let message = match document.status.as_str() {
            "signed" => "This document is already signed.",
            "declined" => "This document was declined.",
            _ => "This document is not ready for signing.",
        };
        return Err(SignerError {
            code: document.status.clone(),
            status: StatusCode::CONFLICT,
            message: message.into(),
        });
    }
    Ok(())
}

struct Authorized {
    document: Snapshot,
    reviewed: bool,
}

/// The document this session may act on (`authorize`).
async fn authorize<'e>(
    executor: impl sqlx::MySqlExecutor<'e>,
    document_id: &str,
    raw_session: &str,
    lock: bool,
) -> SignerResult<Authorized> {
    let sql = format!(
        "SELECT {SNAPSHOT_COLUMNS}, ns.expires_at AS session_expires_at, ns.reviewed_at
           FROM native_signature_documents nd
           JOIN signature_requests sr ON sr.id = nd.request_id AND sr.company_id = nd.company_id
           JOIN native_signature_sessions ns ON ns.request_id = nd.request_id
                AND ns.company_id = nd.company_id AND ns.invitation_token_hash = nd.token_hash
          WHERE nd.document_id = ? AND ns.session_hash = ? AND sr.provider = 'native'
                AND sr.provider_document_id = nd.document_id LIMIT 1{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    let row: Option<SnapshotRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(document_id)
        .bind(hash(raw_session))
        .fetch_optional(executor)
        .await?;
    let row = row.ok_or_else(invalid_link)?;
    let document = parse_snapshot(&row).ok_or_else(invalid_link)?;
    if row.session_expires_at.is_none_or(|at| at <= now()) {
        return Err(invalid_link());
    }
    supported(&document)?;
    Ok(Authorized {
        document,
        reviewed: row.reviewed_at.is_some(),
    })
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Claim {
    idempotency_key: String,
    submission_sha256: String,
    submission_json: String,
    audit_json: String,
    state: String,
    lease_owner: Option<String>,
    lease_until: Option<NaiveDateTime>,
    completed_object_key: Option<String>,
    completed_sha256: Option<String>,
}

async fn claim<'e>(
    executor: impl sqlx::MySqlExecutor<'e>,
    document: &Snapshot,
) -> SignerResult<Option<Claim>> {
    Ok(sqlx::query_as(
        "SELECT idempotency_key, submission_sha256, CAST(submission_json AS CHAR) AS submission_json,
                CAST(audit_json AS CHAR) AS audit_json, CAST(state AS CHAR) AS state, lease_owner,
                lease_until, completed_object_key, completed_sha256
           FROM native_signature_finalizations WHERE request_id = ? AND company_id = ?",
    )
    .bind(document.request_id)
    .bind(document.company_id)
    .fetch_optional(executor)
    .await?)
}

async fn event(
    tx: &mut Transaction<'_, MySql>,
    document: &Snapshot,
    key: &str,
    evidence: &Value,
) -> SignerResult<()> {
    sqlx::query(
        "INSERT INTO native_signature_audit (request_id, company_id, event_key, event_type, evidence_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(document.request_id)
    .bind(document.company_id)
    .bind(key)
    .bind(key)
    .bind(evidence.to_string())
    .bind(now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn iso(at: NaiveDateTime) -> String {
    at.and_utc().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Marks the request viewed (first time only): status, `viewed_at`, audit.
async fn mark_viewed(tx: &mut Transaction<'_, MySql>, document: &Snapshot) -> SignerResult<bool> {
    if document.status != "sent" {
        return Ok(false);
    }
    let at = now();
    sqlx::query(
        "UPDATE signature_requests SET status = 'viewed'
          WHERE id = ? AND company_id = ? AND status = 'sent' AND provider = 'native'",
    )
    .bind(document.request_id)
    .bind(document.company_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE native_signature_documents SET viewed_at = COALESCE(viewed_at, ?)
          WHERE request_id = ? AND company_id = ?",
    )
    .bind(at)
    .bind(document.request_id)
    .bind(document.company_id)
    .execute(&mut **tx)
    .await?;
    event(
        tx,
        document,
        "viewed",
        &serde_json::json!({ "viewedAt": iso(at) }),
    )
    .await?;
    Ok(true)
}

/// Body of `POST /v1/public/signing/{document_id}/session`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InvitationToken {
    /// `token` from the invitation link.
    pub token: String,
}

/// Open a signing session
///
/// Trades the invitation link's token for a 30-minute signing session
/// (`__Host-native_signing_<document>` cookie of this host). Opening the link
/// counts as the signer viewing the document (the sender is told once).
#[utoipa::path(
    post,
    path = "/v1/public/signing/{document_id}/session",
    operation_id = "open_signing_session",
    tag = "signing",
    params(("document_id" = String, Path, description = "The document's id (a UUID)")),
    request_body = InvitationToken,
    responses(
        (status = 204, description = "Session open (cookie set)"),
        (status = 404, description = "Not a valid link", body = SignerErrorBody),
        (status = 409, description = "Not ready for signing", body = SignerErrorBody),
        (status = 410, description = "The link has expired", body = SignerErrorBody),
        (status = 503, description = "Signing is unavailable", body = SignerErrorBody),
    )
)]
pub async fn open_session(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
    Json(input): Json<InvitationToken>,
) -> Response {
    match exchange(&state, &document_id, &input.token).await {
        Ok(raw) => {
            let cookie = format!(
                "{}={raw}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
                cookie_name(&document_id),
                SESSION_MINUTES * 60
            );
            let mut response = StatusCode::NO_CONTENT.into_response();
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            no_store(response)
        }
        Err(error) => error.into_response(),
    }
}

async fn exchange(state: &AppState, document_id: &str, token: &str) -> SignerResult<String> {
    secret(state)?;
    if !valid_uuid(document_id) || !valid_token(token) {
        return Err(invalid_link());
    }
    let raw: String = {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| unavailable())?;
        URL_SAFE_NO_PAD.encode(bytes)
    };
    let mut tx = state.pool.begin().await?;
    let sql = format!(
        "SELECT {SNAPSHOT_COLUMNS}
           FROM native_signature_documents nd
           JOIN signature_requests sr ON sr.id = nd.request_id AND sr.company_id = nd.company_id
          WHERE nd.document_id = ? AND nd.token_hash = ? AND sr.provider = 'native'
                AND sr.provider_document_id = nd.document_id LIMIT 1 FOR UPDATE"
    );
    let row: Option<SnapshotRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(document_id)
        .bind(hash(token))
        .fetch_optional(&mut *tx)
        .await?;
    let document = row
        .as_ref()
        .and_then(parse_snapshot)
        .ok_or_else(invalid_link)?;
    supported(&document)?;
    let pending = claim(&mut *tx, &document).await?;
    if document.status != "signed" && document.status != "declined" && pending.is_none() {
        active(&document)?;
    }
    sqlx::query(
        "INSERT INTO native_signature_sessions
           (session_hash, request_id, company_id, invitation_token_hash, expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(hash(&raw))
    .bind(document.request_id)
    .bind(document.company_id)
    .bind(hash(token))
    .bind(now() + chrono::Duration::minutes(SESSION_MINUTES))
    .bind(now())
    .execute(&mut *tx)
    .await?;
    let first_view = mark_viewed(&mut tx, &document).await?;
    tx.commit().await?;
    if first_view {
        notify_status(
            state,
            i64::from(document.request_id),
            document.company_id,
            "viewed",
        )
        .await;
    }
    Ok(raw)
}

/// A field as the signer sees it.
#[derive(Debug, Serialize, ToSchema)]
pub struct SigningView {
    /// The document's id.
    pub document_id: String,
    /// The prepared PDF's SHA-256; send it back with the answers.
    pub document_version: String,
    /// Document name.
    pub filename: String,
    /// Who signs.
    pub signer_name: String,
    /// `sent`, `viewed`, `signed`, `declined` or `expired`.
    pub status: String,
    /// The fields; those without `value` (and not `autofill_date_signed`)
    /// are the signer's to fill.
    pub fields: Vec<SignatureField>,
    /// How the signer may sign: `type` and/or `draw`.
    pub signature_methods: Vec<String>,
    /// The signer may decline.
    pub allow_decline: bool,
    /// The signer pressed start in this session (required before finishing).
    pub reviewed: bool,
    /// A finish was recorded but not completed: resume it.
    pub finish_pending: bool,
    /// Send it with every action.
    pub csrf: String,
    /// The consent the signer agrees to.
    pub consent_version: String,
    /// Its wording.
    pub consent_text: String,
    /// The document to read (API path, on this host).
    pub prepared_url: String,
    /// The signed copy (API path), once signed.
    pub completed_url: Option<String>,
}

/// What the signing page shows.
#[derive(Debug, Serialize, ToSchema)]
pub struct SigningPage {
    /// `native` (signed here, see `view`) or `signwell` (embed `signing_url`).
    pub provider: String,
    /// A native document and the signer's state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<Box<SigningView>>,
    /// `SignWell`'s signing page for the embed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_url: Option<String>,
}

impl SigningPage {
    fn native(view: SigningView) -> Self {
        Self {
            provider: "native".into(),
            view: Some(Box::new(view)),
            signing_url: None,
        }
    }
}

async fn view_for(state: &AppState, document_id: &str, raw: &str) -> SignerResult<SigningView> {
    let secret = secret(state)?;
    let Authorized { document, reviewed } = authorize(&state.pool, document_id, raw, false).await?;
    let pending = claim(&state.pool, &document).await?;
    let is_expired = document.status != "signed"
        && document.status != "declined"
        && pending.is_none()
        && document.expires_at.is_some_and(|at| at <= now());
    let base = format!("/v1/public/signing/{}/download", document.document_id);
    Ok(SigningView {
        completed_url: (document.status == "signed"
            && document.completed_object_key.is_some()
            && document.completed_sha256.is_some())
        .then(|| format!("{base}?kind=completed")),
        prepared_url: format!("{base}?kind=prepared"),
        document_id: document.document_id.clone(),
        document_version: document.document_version.clone(),
        filename: document.filename.clone(),
        signer_name: document.signer_name.clone(),
        status: if is_expired {
            "expired".into()
        } else {
            document.status.clone()
        },
        signature_methods: document.settings["signatureMethods"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|m| matches!(*m, "type" | "draw"))
            .map(str::to_string)
            .collect(),
        allow_decline: document.settings["allowDecline"] == Value::Bool(true),
        reviewed,
        finish_pending: pending.is_some_and(|c| c.state != "completed"),
        csrf: csrf_for(secret, raw),
        consent_version: CONSENT_VERSION.into(),
        consent_text: CONSENT_TEXT.into(),
        fields: document.fields,
    })
}

/// Open a signing link
///
/// For a native document: the document and the signer's state (needs the
/// session cookie from `.../session`). For a `SignWell` document: its
/// embedded signing page.
#[utoipa::path(
    get,
    path = "/v1/public/signing/{document_id}",
    operation_id = "get_signing_page",
    tag = "signing",
    params(("document_id" = String, Path, description = "The document's id (a UUID)")),
    responses(
        (status = 200, description = "What to show", body = SigningPage),
        (status = 404, description = "Not a valid link (or no session)", body = SignerErrorBody),
        (status = 503, description = "Signing is unavailable", body = SignerErrorBody),
    )
)]
pub async fn signing_view(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        if !valid_uuid(&document_id) {
            return Err(invalid_link());
        }
        let native: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM signature_requests WHERE provider_document_id = ? AND provider = 'native' LIMIT 1",
        )
        .bind(&document_id)
        .fetch_optional(&state.pool)
        .await?;
        let has_cookie = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.contains(&format!("{}=", cookie_name(&document_id))));
        if native.is_some() || has_cookie {
            let raw = session(&headers, &document_id)?;
            return Ok(SigningPage::native(view_for(&state, &document_id, &raw).await?));
        }
        let company_key: Option<Option<String>> = sqlx::query_scalar(
            "SELECT c.signwell_api_key FROM signature_requests sr JOIN company c ON c.id = sr.company_id
              WHERE sr.provider_document_id = ? AND sr.provider = 'signwell' ORDER BY sr.id DESC LIMIT 1",
        )
        .bind(&document_id)
        .fetch_optional(&state.pool)
        .await?;
        let key = crate::signwell::api_key(company_key.flatten().as_deref())
            .map_err(|_| invalid_link())?;
        let url = crate::signwell::SignWell::new(&state.signwell_base)
            .signing_url(&key, &document_id)
            .await
            .ok_or_else(invalid_link)?;
        Ok(SigningPage { provider: "signwell".into(), view: None, signing_url: Some(url) })
    }
    .await;
    match result {
        Ok(page) => no_store(Json(page).into_response()),
        Err(error) => error.into_response(),
    }
}

/// Body of `POST /v1/public/signing/{document_id}/actions`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SigningAction {
    /// `start` (the signer reviewed the document), `finish` (sign with
    /// `submission`), `resume` (complete an interrupted finish) or `decline`.
    pub intent: String,
    /// The view's `csrf`.
    pub csrf: String,
    /// With `finish`: the answers.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub submission: Option<Value>,
    /// With `decline`: why (up to 500 characters).
    #[serde(default)]
    pub reason: Option<String>,
}

/// Act on a document
///
/// Starts (the signer reviewed it), finishes (signs; answers in
/// `submission`: `documentVersion`, `consent: true`, `consentVersion`,
/// `idempotencyKey`, `answers` by field id), resumes an interrupted finish,
/// or declines. Must come from the signing page's origin with the session
/// cookie and its `csrf`. Answers the updated view.
#[utoipa::path(
    post,
    path = "/v1/public/signing/{document_id}/actions",
    operation_id = "act_on_signing",
    tag = "signing",
    params(("document_id" = String, Path, description = "The document's id (a UUID)")),
    request_body = SigningAction,
    responses(
        (status = 200, description = "The updated view", body = SigningPage),
        (status = 400, description = "Invalid answers", body = SignerErrorBody),
        (status = 403, description = "Wrong origin or CSRF token, or declining is off", body = SignerErrorBody),
        (status = 404, description = "Not a valid link (or no session)", body = SignerErrorBody),
        (status = 409, description = "Already signed or declined, a finish is in progress, or the document changed", body = SignerErrorBody),
        (status = 410, description = "The link has expired", body = SignerErrorBody),
        (status = 413, description = "Too large", body = SignerErrorBody),
        (status = 415, description = "Not JSON", body = SignerErrorBody),
        (status = 503, description = "Unavailable, or the signed PDF could not be saved (resume)", body = SignerErrorBody),
    )
)]
#[allow(clippy::too_many_lines)]
pub async fn signing_action(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let raw = session(&headers, &document_id)?;
        let json_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim() == "application/json");
        if !json_type {
            return Err(SignerError::new(
                "invalid_submission",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "A JSON signing action is required.",
            ));
        }
        let bytes = axum::body::to_bytes(body, MAX_ACTION_BYTES)
            .await
            .map_err(|_| {
                SignerError::new(
                    "invalid_submission",
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "The signing action is too large.",
                )
            })?;
        let action: SigningAction = serde_json::from_slice(&bytes).map_err(|_| {
            SignerError::new(
                "invalid_submission",
                StatusCode::BAD_REQUEST,
                "The signing action is invalid.",
            )
        })?;
        let secret = secret(&state)?;
        let origin_ok = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|origin| origin == &*state.signature_app_origin);
        let expected = csrf_for(secret, &raw);
        let csrf_ok = action.csrf.len() == expected.len()
            && action
                .csrf
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if !origin_ok || !csrf_ok {
            return Err(SignerError::new(
                "csrf",
                StatusCode::FORBIDDEN,
                "Please reopen the signing link and try again.",
            ));
        }
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        match action.intent.as_str() {
            "start" => {
                record_review(&state, &document_id, &raw).await?;
            }
            "decline" => {
                decline(
                    &state,
                    &document_id,
                    &raw,
                    action.reason.as_deref().unwrap_or(""),
                )
                .await?;
            }
            "finish" => {
                finish(
                    &state,
                    &document_id,
                    &raw,
                    action.submission.as_ref(),
                    user_agent,
                    false,
                )
                .await?;
            }
            "resume" => {
                finish(
                    &state,
                    &document_id,
                    &raw,
                    action.submission.as_ref(),
                    user_agent,
                    true,
                )
                .await?;
            }
            _ => {
                return Err(SignerError::new(
                    "invalid_submission",
                    StatusCode::BAD_REQUEST,
                    "Check the required fields and consent, then try again.",
                ));
            }
        }
        Ok(SigningPage::native(
            view_for(&state, &document_id, &raw).await?,
        ))
    }
    .await;
    match result {
        Ok(page) => no_store(Json(page).into_response()),
        Err(error) => error.into_response(),
    }
}

async fn record_review(state: &AppState, document_id: &str, raw: &str) -> SignerResult<()> {
    let mut tx = state.pool.begin().await?;
    let Authorized { document, .. } = authorize(&mut *tx, document_id, raw, true).await?;
    active(&document)?;
    sqlx::query("UPDATE native_signature_sessions SET reviewed_at = COALESCE(reviewed_at, ?) WHERE session_hash = ?")
        .bind(now())
        .bind(hash(raw))
        .execute(&mut *tx)
        .await?;
    let first_view = mark_viewed(&mut tx, &document).await?;
    tx.commit().await?;
    if first_view {
        notify_status(
            state,
            i64::from(document.request_id),
            document.company_id,
            "viewed",
        )
        .await;
    }
    Ok(())
}

async fn decline(state: &AppState, document_id: &str, raw: &str, reason: &str) -> SignerResult<()> {
    let reason = reason.trim();
    if reason.chars().count() > 500 {
        return Err(SignerError::new(
            "invalid_submission",
            StatusCode::BAD_REQUEST,
            "Check the required fields and consent, then try again.",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let Authorized { document, .. } = authorize(&mut *tx, document_id, raw, true).await?;
    if document.status == "declined" {
        return Ok(());
    }
    active(&document)?;
    if document.settings["allowDecline"] != Value::Bool(true) {
        return Err(SignerError::new(
            "decline_disabled",
            StatusCode::FORBIDDEN,
            "Declining is unavailable for this document.",
        ));
    }
    if claim(&mut *tx, &document).await?.is_some() {
        return Err(SignerError::new(
            "processing",
            StatusCode::CONFLICT,
            "Signing has started. Please retry completion.",
        ));
    }
    sqlx::query(
        "UPDATE signature_requests SET status = 'declined' WHERE id = ? AND company_id = ? AND provider = 'native'",
    )
    .bind(document.request_id)
    .bind(document.company_id)
    .execute(&mut *tx)
    .await?;
    event(
        &mut tx,
        &document,
        "declined",
        &serde_json::json!({ "reason": reason, "declinedAt": iso(now()) }),
    )
    .await?;
    tx.commit().await?;
    notify_status(
        state,
        i64::from(document.request_id),
        document.company_id,
        "declined",
    )
    .await;
    Ok(())
}

/// The signer's answers (`SignerSubmission`), checked (`validateNativeSubmission`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Submission {
    document_version: String,
    idempotency_key: String,
    answers: BTreeMap<String, String>,
}

impl Submission {
    /// `JSON.stringify` of the normalized submission (answers sorted), in
    /// the CRM's key order, so claims hash the same.
    fn canonical_json(&self) -> String {
        format!(
            "{{\"documentVersion\":{},\"consent\":true,\"consentVersion\":{},\"idempotencyKey\":{},\"answers\":{}}}",
            Value::String(self.document_version.clone()),
            Value::String(CONSENT_VERSION.into()),
            Value::String(self.idempotency_key.clone()),
            serde_json::to_string(&self.answers).expect("answers serialize"),
        )
    }
}

fn invalid_submission() -> SignerError {
    SignerError::new(
        "invalid_submission",
        StatusCode::BAD_REQUEST,
        "Check the required fields and consent, then try again.",
    )
}

fn check_submission(input: &Value, document: &Snapshot) -> SignerResult<Submission> {
    let object = input.as_object().ok_or_else(invalid_submission)?;
    let allowed = [
        "documentVersion",
        "consent",
        "consentVersion",
        "idempotencyKey",
        "answers",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_submission());
    }
    let version = object["documentVersion"].as_str().unwrap_or_default();
    let key = object["idempotencyKey"].as_str().unwrap_or_default();
    let key_ok = (16..=64).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if version.len() != 64
        || !version
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || object.get("consent") != Some(&Value::Bool(true))
        || object.get("consentVersion").and_then(Value::as_str) != Some(CONSENT_VERSION)
        || !key_ok
    {
        return Err(invalid_submission());
    }
    let mut answers = BTreeMap::new();
    for (id, value) in object
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(invalid_submission)?
    {
        let value = value.as_str().ok_or_else(invalid_submission)?;
        if id.chars().count() > 64 || value.chars().count() > 300_000 {
            return Err(invalid_submission());
        }
        answers.insert(id.clone(), value.to_string());
    }
    if version != document.document_version {
        return Err(invalid_submission());
    }
    let editable: Vec<&SignatureField> = document
        .fields
        .iter()
        .filter(|f| {
            f.value.as_deref().is_none_or(|v| v.trim().is_empty())
                && f.kind != FieldKind::AutofillDateSigned
        })
        .collect();
    let ids: HashSet<&str> = editable.iter().filter_map(|f| f.id.as_deref()).collect();
    if answers.keys().any(|id| !ids.contains(id.as_str()))
        || editable.iter().any(|f| {
            f.required
                && f.id
                    .as_deref()
                    .and_then(|id| answers.get(id))
                    .is_none_or(|v| v.trim().is_empty())
        })
    {
        return Err(invalid_submission());
    }
    Ok(Submission {
        document_version: version.to_string(),
        idempotency_key: key.to_string(),
        answers,
    })
}

fn scoped_key(document: &Snapshot, kind: &str, sha: &str) -> String {
    format!(
        "{}/signatures/{}/{kind}-{sha}.pdf",
        document.company_id, document.document_id
    )
}

async fn read_pdf(
    state: &AppState,
    document: &Snapshot,
    key: &str,
    sha: &str,
) -> SignerResult<Vec<u8>> {
    if key != scoped_key(document, "prepared", sha) && key != scoped_key(document, "completed", sha)
    {
        return Err(unavailable());
    }
    let bucket = state.signature_bucket.as_deref().ok_or_else(unavailable)?;
    match state.storage.get_url(&format!("s3://{bucket}/{key}")).await {
        Ok(Some(object)) if sha256_hex(&object.bytes) == sha => Ok(object.bytes),
        Ok(_) => Err(unavailable()),
        Err(error) => {
            tracing::error!(?error, "native signature read failed");
            Err(unavailable())
        }
    }
}

fn retryable() -> SignerError {
    SignerError::new(
        "retryable_finalization",
        StatusCode::SERVICE_UNAVAILABLE,
        "Completion needs recovery. Please retry.",
    )
}

/// `finishNativeSigning`.
#[allow(clippy::too_many_lines)]
async fn finish(
    state: &AppState,
    document_id: &str,
    raw: &str,
    input: Option<&Value>,
    user_agent: &str,
    resume: bool,
) -> SignerResult<()> {
    let initial = authorize(&state.pool, document_id, raw, false).await?;
    let previous = claim(&state.pool, &initial.document).await?;
    let (submission, audit) = if let Some(previous) = &previous {
        let stored: Value =
            serde_json::from_str(&previous.submission_json).map_err(|_| retryable())?;
        let submission = check_submission(&stored, &initial.document)?;
        let audit: SigningAudit =
            serde_json::from_str(&previous.audit_json).map_err(|_| retryable())?;
        let incoming = if resume {
            submission.clone()
        } else {
            check_submission(input.ok_or_else(invalid_submission)?, &initial.document)?
        };
        if hash(&incoming.canonical_json()) != previous.submission_sha256 {
            return Err(SignerError::new(
                "idempotency_conflict",
                StatusCode::CONFLICT,
                "A different finish is already recorded. Resume the original completion.",
            ));
        }
        (submission, audit)
    } else {
        active(&initial.document)?;
        if resume {
            return Err(SignerError::new(
                "no_claim",
                StatusCode::CONFLICT,
                "No interrupted completion was found.",
            ));
        }
        if !initial.reviewed {
            return Err(SignerError::new(
                "review_required",
                StatusCode::CONFLICT,
                "Please review the document before signing.",
            ));
        }
        let submission =
            check_submission(input.ok_or_else(invalid_submission)?, &initial.document)?;
        if user_agent.chars().count() > 2000 {
            return Err(invalid_submission());
        }
        let audit = SigningAudit {
            document_id: initial.document.document_id.clone(),
            prepared_sha256: initial.document.prepared_sha256.clone(),
            signer_name: initial.document.signer_name.clone(),
            signer_email: initial.document.signer_email.clone(),
            signed_at: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            consent_version: CONSENT_VERSION.into(),
            ip_address: "unavailable".into(),
            user_agent: user_agent.to_string(),
            settings: initial.document.settings.clone(),
        };
        (submission, audit)
    };
    let submission_hash = hash(&submission.canonical_json());
    if previous.as_ref().is_some_and(|c| c.state == "completed") {
        return receipt(&initial.document, previous.as_ref().expect("checked"));
    }
    let prepared = read_pdf(
        state,
        &initial.document,
        &initial.document.prepared_object_key,
        &initial.document.prepared_sha256,
    )
    .await?;
    // Checked before any claim: invalid answers can still be corrected.
    signing_pdf::validate_native_input(
        &prepared,
        &initial.document.fields,
        &submission.answers,
        &audit,
    )?;
    let owner = uuid::Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await?;
    let Authorized { document, reviewed } = authorize(&mut *tx, document_id, raw, true).await?;
    if document.document_version != submission.document_version {
        return Err(SignerError::new(
            "version_changed",
            StatusCode::CONFLICT,
            "The document changed. Please reopen the signing link.",
        ));
    }
    if let Some(existing) = claim(&mut *tx, &document).await? {
        if existing.idempotency_key != submission.idempotency_key
            || existing.submission_sha256 != submission_hash
        {
            return Err(SignerError::new(
                "idempotency_conflict",
                StatusCode::CONFLICT,
                "A different finish is already recorded. Resume the original completion.",
            ));
        }
        if existing.state == "completed" {
            return receipt(&document, &existing);
        }
        if existing.lease_until.is_some_and(|until| until > now()) {
            return Err(SignerError::new(
                "processing",
                StatusCode::CONFLICT,
                "Completion is in progress. Please retry shortly.",
            ));
        }
        let recorded: Option<Value> = serde_json::from_str(&existing.audit_json).ok();
        if recorded != serde_json::to_value(&audit).ok() {
            return Err(SignerError::new(
                "processing",
                StatusCode::CONFLICT,
                "An original finish was recorded. Please retry completion.",
            ));
        }
        if !matches!(document.status.as_str(), "sent" | "viewed") {
            active(&document)?;
        }
    } else {
        active(&document)?;
        if !reviewed {
            return Err(SignerError::new(
                "review_required",
                StatusCode::CONFLICT,
                "Please review the document before signing.",
            ));
        }
        let signed_at = chrono::DateTime::parse_from_rfc3339(&audit.signed_at)
            .map_or_else(|_| now(), |at| at.naive_utc());
        sqlx::query(
            "INSERT INTO native_signature_finalizations
               (request_id, company_id, idempotency_key, submission_sha256, submission_json, audit_json, signed_at, state)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'retryable')",
        )
        .bind(document.request_id)
        .bind(document.company_id)
        .bind(&submission.idempotency_key)
        .bind(&submission_hash)
        .bind(submission.canonical_json())
        .bind(serde_json::to_string(&audit).expect("audit serializes"))
        .bind(signed_at)
        .execute(&mut *tx)
        .await?;
        event(
            &mut tx,
            &document,
            "consent",
            &serde_json::json!({
                "consentVersion": CONSENT_VERSION,
                "consentText": CONSENT_TEXT,
                "signedAt": audit.signed_at,
                "documentVersion": submission.document_version,
                "submissionSha256": submission_hash,
            }),
        )
        .await?;
    }
    sqlx::query(
        "UPDATE native_signature_finalizations SET state = 'rendering', lease_owner = ?, lease_until = ?
          WHERE request_id = ? AND company_id = ?",
    )
    .bind(&owner)
    .bind(now() + chrono::Duration::seconds(LEASE_SECONDS))
    .bind(document.request_id)
    .bind(document.company_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    match complete(
        state,
        document_id,
        raw,
        &document,
        &prepared,
        &submission,
        &audit,
        &owner,
    )
    .await
    {
        Ok(()) => {
            notify_status(
                state,
                i64::from(document.request_id),
                document.company_id,
                "signed",
            )
            .await;
            email_signed_copy(state, document.request_id, document.company_id).await;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query(
                "UPDATE native_signature_finalizations SET state = 'retryable', lease_owner = NULL, lease_until = NULL
                  WHERE request_id = ? AND company_id = ? AND lease_owner = ?",
            )
            .bind(document.request_id)
            .bind(document.company_id)
            .bind(&owner)
            .execute(&state.pool)
            .await;
            if error.code == "processing" {
                return Err(error);
            }
            Err(SignerError::new(
                "retryable_finalization",
                StatusCode::SERVICE_UNAVAILABLE,
                "The PDF could not be saved. Your finish is recorded; please retry completion.",
            ))
        }
    }
}

fn receipt(document: &Snapshot, claim: &Claim) -> SignerResult<()> {
    if claim.completed_object_key.is_none()
        || claim.completed_sha256.is_none()
        || document.status != "signed"
        || document.completed_object_key != claim.completed_object_key
        || document.completed_sha256 != claim.completed_sha256
    {
        return Err(retryable());
    }
    Ok(())
}

/// Renders and stores the signed copy, then marks the request signed.
#[allow(clippy::too_many_arguments)]
async fn complete(
    state: &AppState,
    document_id: &str,
    raw: &str,
    document: &Snapshot,
    prepared: &[u8],
    submission: &Submission,
    audit: &SigningAudit,
    owner: &str,
) -> SignerResult<()> {
    let signed =
        signing_pdf::build_native_signed(prepared, &document.fields, &submission.answers, audit)
            .map_err(|_| unavailable())?;
    let bucket = state.signature_bucket.as_deref().ok_or_else(unavailable)?;
    if signed.evidence["documentId"] != document.document_id.as_str()
        || signed.evidence["completedSha256"] != signed.sha256.as_str()
    {
        return Err(unavailable());
    }
    let evidence = serde_json::to_vec(&signed.evidence).expect("evidence serializes");
    let prefix = format!(
        "{}/signatures/{}/",
        document.company_id, document.document_id
    );
    let object_key = format!("{prefix}completed-{}.pdf", signed.sha256);
    let evidence_key = format!("{prefix}audit-{}.json", sha256_hex(&evidence));
    if let Err(error) = state.storage.verify_private(bucket).await {
        tracing::error!(?error, "signature bucket is not verified private");
        return Err(unavailable());
    }
    state
        .storage
        .put_private(bucket, &object_key, signed.bytes.clone(), "application/pdf")
        .await
        .map_err(|_| unavailable())?;
    state
        .storage
        .put_private(bucket, &evidence_key, evidence, "application/json")
        .await
        .map_err(|_| unavailable())?;
    let mut tx = state.pool.begin().await?;
    let Authorized {
        document: current, ..
    } = authorize(&mut *tx, document_id, raw, true).await?;
    let held = claim(&mut *tx, &current).await?;
    if held.as_ref().and_then(|c| c.lease_owner.as_deref()) != Some(owner)
        || !matches!(current.status.as_str(), "sent" | "viewed")
    {
        return Err(SignerError::new(
            "processing",
            StatusCode::CONFLICT,
            "Completion ownership changed. Please retry.",
        ));
    }
    let signed_at = chrono::DateTime::parse_from_rfc3339(&audit.signed_at)
        .map_or_else(|_| now(), |at| at.naive_utc());
    sqlx::query(
        "UPDATE native_signature_documents SET completed_object_key = ?, completed_sha256 = ?, signed_at = ?
          WHERE request_id = ? AND company_id = ?",
    )
    .bind(&object_key)
    .bind(&signed.sha256)
    .bind(signed_at)
    .bind(current.request_id)
    .bind(current.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE signature_requests SET status = 'signed' WHERE id = ? AND company_id = ? AND provider = 'native'",
    )
    .bind(current.request_id)
    .bind(current.company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE native_signature_finalizations SET state = 'completed', lease_owner = NULL, lease_until = NULL,
                completed_object_key = ?, completed_sha256 = ?, evidence_object_key = ?
          WHERE request_id = ? AND company_id = ? AND lease_owner = ?",
    )
    .bind(&object_key)
    .bind(&signed.sha256)
    .bind(&evidence_key)
    .bind(current.request_id)
    .bind(current.company_id)
    .bind(owner)
    .execute(&mut *tx)
    .await?;
    let mut completed = signed.evidence.clone();
    completed["evidenceObjectKey"] = Value::String(evidence_key);
    event(&mut tx, &current, "completed", &completed).await?;
    tx.commit().await?;
    Ok(())
}

/// Emails the signer their signed copy (`emailSignedCopyToSigner`); a
/// failure loses only the email.
async fn email_signed_copy(state: &AppState, request_id: i32, company_id: i32) {
    #[derive(sqlx::FromRow)]
    struct Row {
        signer_email: String,
        filename: String,
        completed_object_key: Option<String>,
        sender_email: Option<String>,
        company_name: Option<String>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT sr.signer_email, sr.filename, nd.completed_object_key,
                u.email AS sender_email, c.name AS company_name
           FROM signature_requests sr
           JOIN native_signature_documents nd ON nd.request_id = sr.id AND nd.company_id = sr.company_id
           LEFT JOIN users u ON u.id = sr.created_by_user_id
           LEFT JOIN company c ON c.id = sr.company_id
          WHERE sr.id = ? AND sr.company_id = ? AND sr.status = 'signed' LIMIT 1",
    )
    .bind(request_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(row) = row else { return };
    let (Some(key), Some(bucket)) = (row.completed_object_key, state.signature_bucket.as_deref())
    else {
        return;
    };
    let to = row.signer_email.trim();
    if to.is_empty() {
        return;
    }
    let Ok(url) = state
        .storage
        .presign_get_in(Some(bucket), &key, "application/pdf", Some(&row.filename))
        .await
    else {
        return;
    };
    let title = row
        .filename
        .len()
        .checked_sub(4)
        .filter(|&cut| {
            row.filename.is_char_boundary(cut) && row.filename[cut..].eq_ignore_ascii_case(".pdf")
        })
        .map_or_else(
            || row.filename.clone(),
            |cut| row.filename[..cut].to_string(),
        );
    let escaped = crate::routes::bug_reports::escape(&title);
    let body = format!(
        "<div style=\"font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto; text-align: center; color: #1e293b;\">\n  <p style=\"font-size: 18px; margin: 24px 0 4px;\">You're done signing</p>\n  <p style=\"font-size: 16px; margin: 0 0 24px;\">{escaped}</p>\n  <p style=\"color: #475569; line-height: 1.6;\">Attached is the final agreement for your reference.</p>\n</div>"
    );
    let company = row
        .company_name
        .as_deref()
        .map(|name| name.replace(['"', '\r', '\n'], "").trim().to_string())
        .filter(|name| !name.is_empty());
    let email = OutgoingEmail {
        from_name: company,
        from_address: "noreply@granite-manager.com".into(),
        to: vec![to.to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("You signed: \"{title}\""),
        body,
        signature: None,
        reply_to: row
            .sender_email
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .into_iter()
            .collect(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: vec![OutgoingAttachment {
            filename: row.filename,
            content_type: "application/pdf".into(),
            url,
        }],
        transactional: true,
    };
    if let Err(error) = state.webhooks.send_email(&email).await {
        tracing::warn!(
            ?error,
            request_id,
            "could not email the signer their signed copy"
        );
    }
}

/// `?kind=` of the download.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DownloadKind {
    /// `prepared` (the document to read) or `completed` (the signed copy).
    #[serde(default)]
    pub kind: String,
}

/// Download the document
///
/// `prepared`: the document to review (while it can be signed), shown
/// inline. `completed`: the signed copy, as an attachment. Needs the
/// session cookie. Big files redirect to a short-lived link.
#[utoipa::path(
    get,
    path = "/v1/public/signing/{document_id}/download",
    operation_id = "download_signing_document",
    tag = "signing",
    params(("document_id" = String, Path, description = "The document's id (a UUID)"), DownloadKind),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf"),
        (status = 302, description = "A short-lived link to a big PDF"),
        (status = 404, description = "Not a valid link, kind or session", body = SignerErrorBody),
        (status = 409, description = "Not signable any more", body = SignerErrorBody),
        (status = 410, description = "Expired", body = SignerErrorBody),
        (status = 503, description = "Unavailable", body = SignerErrorBody),
    )
)]
pub async fn download(
    State(state): State<AppState>,
    Path(document_id): Path<String>,
    Query(query): Query<DownloadKind>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let raw = session(&headers, &document_id)?;
        let Authorized { document, .. } = authorize(&state.pool, &document_id, &raw, false).await?;
        let (key, sha) = match query.kind.as_str() {
            "prepared" => {
                active(&document)?;
                (
                    document.prepared_object_key.clone(),
                    document.prepared_sha256.clone(),
                )
            }
            "completed" => match (&document.completed_object_key, &document.completed_sha256) {
                (Some(key), Some(sha)) if document.status == "signed" => (key.clone(), sha.clone()),
                _ => return Err(invalid_link()),
            },
            _ => return Err(invalid_link()),
        };
        let bytes = read_pdf(&state, &document, &key, &sha).await?;
        let disposition = if query.kind == "prepared" {
            "inline"
        } else {
            "attachment"
        };
        if bytes.len() > MAX_INLINE_BYTES {
            let bucket = state.signature_bucket.as_deref().ok_or_else(unavailable)?;
            let link = state
                .storage
                .presign_get_in(
                    Some(bucket),
                    &key,
                    "application/pdf",
                    Some(&document.filename),
                )
                .await
                .map_err(|_| unavailable())?;
            return Ok((StatusCode::FOUND, [(header::LOCATION, link)]).into_response());
        }
        let filename: String = document
            .filename
            .chars()
            .map(|c| {
                if matches!(c, '\r' | '\n' | '"' | '\\' | '/') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let encoded =
            percent_encoding::utf8_percent_encode(&filename, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        Ok((
            [
                (header::CONTENT_TYPE, "application/pdf".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("{disposition}; filename=\"document.pdf\"; filename*=UTF-8''{encoded}"),
                ),
            ],
            bytes,
        )
            .into_response())
    }
    .await;
    match result {
        Ok(response) => no_store(response),
        Err(error) => error.into_response(),
    }
}

/// For tests: the CSRF token of a session.
pub fn csrf_token(secret: &str, raw_session: &str) -> String {
    csrf_for(secret, raw_session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_bytes_of_base64url() {
        assert!(valid_token(&URL_SAFE_NO_PAD.encode([7u8; 32])));
        assert!(!valid_token("short"));
        assert!(!valid_token(&URL_SAFE_NO_PAD.encode([7u8; 31])));
    }

    #[test]
    fn the_canonical_submission_matches_the_crm() {
        let submission = Submission {
            document_version: "a".repeat(64),
            idempotency_key: "abcdefghijklmnop".into(),
            answers: BTreeMap::from([("b".into(), "2".into()), ("a".into(), "1".into())]),
        };
        assert_eq!(
            submission.canonical_json(),
            format!(
                "{{\"documentVersion\":\"{}\",\"consent\":true,\"consentVersion\":\"native-v1\",\"idempotencyKey\":\"abcdefghijklmnop\",\"answers\":{{\"a\":\"1\",\"b\":\"2\"}}}}",
                "a".repeat(64)
            )
        );
    }
}
