//! A customer's own page (no CRM session): their projects and invoices,
//! unlocked by a one-time code emailed to the address on file.
//!
//! - `GET    /v1/public/customer-view/{company_id}/{view_id}`          → the gate: verified, and where a code goes
//! - `POST   /v1/public/customer-view/{company_id}/{view_id}/code`     → email a code
//! - `POST   /v1/public/customer-view/{company_id}/{view_id}/verify`   → enter it
//! - `GET    /v1/public/customer-view/{company_id}/{view_id}/projects` → their sales
//! - `GET    /v1/public/customer-view/{company_id}/{view_id}/quotes`   → their online quotes
//! - `GET    /v1/public/customer-view/{company_id}/{view_id}/invoices` → their `QuickBooks` invoices
//! - `GET    /v1/public/customer-view/{company_id}/{view_id}/invoices/{invoice_id}/pdf`
//! - `GET    /v1/public/customer-view/{company_id}/login`              → a pending sign-in by email
//! - `POST   /v1/public/customer-view/{company_id}/login/code`         → email a code to the account's address
//! - `POST   /v1/public/customer-view/{company_id}/login/verify`       → enter it; answers the page to open
//! - `DELETE /v1/public/customer-view/login`                           → drop a pending code
//!
//! The state lives in the signed `__customer_view` cookie (same format and
//! secret as the CRM's, so a verified visitor stays verified for 7 days).
//! Port of `customerViewAuth.server.ts` and the `customer.$company.$viewId`
//! and `customer.$company.stones.account` routes.

use crate::auth::cookie::{cookie_value, decode_session_map, encode_session_map};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::routes::quickbooks::{CustomerInvoices, customer_invoice_pdf_of, invoices_of_customer};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hmac::digest::Digest;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(view_gate))
        .routes(routes!(send_view_code))
        .routes(routes!(verify_view_code))
        .routes(routes!(projects))
        .routes(routes!(quotes))
        .routes(routes!(invoices))
        .routes(routes!(invoice_pdf))
        .routes(routes!(pending_login))
        .routes(routes!(send_login_code))
        .routes(routes!(verify_login_code))
        .routes(routes!(cancel_code))
}

const COOKIE: &str = "__customer_view";
const CODE_EXPIRY_MS: i64 = 10 * 60 * 1000;
const SEND_COOLDOWN_MS: i64 = 45_000;
const MAX_VERIFY_ATTEMPTS: i64 = 8;
const VERIFIED_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 7;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The visitor's state (`CustomerViewSessionData`), and whether it changed.
struct Visitor {
    data: Map<String, Value>,
    changed: bool,
}

impl Visitor {
    fn read(state: &AppState, headers: &HeaderMap) -> Self {
        let data = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|cookies| cookie_value(cookies, COOKIE))
            .and_then(|value| decode_session_map(&value, &state.session_secret))
            .unwrap_or_default();
        Self {
            data,
            changed: false,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.data.get(key).and_then(Value::as_i64)
    }

    fn set(&mut self, key: &str, value: Value) {
        self.data.insert(key.to_string(), value);
        self.changed = true;
    }

    fn unset(&mut self, key: &str) {
        self.changed |= self.data.remove(key).is_some();
    }

    fn clear_pending(&mut self) {
        for key in [
            "pendingViewId",
            "pendingCompanyId",
            "pendingEmail",
            "pendingCodeHash",
            "pendingExpiresAt",
            "pendingAttempts",
            "pendingLastSentAt",
            "pendingDummy",
        ] {
            self.unset(key);
        }
    }

    fn verified(&self, view_id: &str, company_id: i32) -> bool {
        self.int("verifiedAt")
            .is_some_and(|at| now_ms() - at <= VERIFIED_MAX_AGE_SECONDS * 1000)
            && self.str("verifiedViewId") == Some(view_id)
            && self.int("verifiedCompanyId") == Some(i64::from(company_id))
    }

    fn pending_live(&self) -> bool {
        self.int("pendingExpiresAt").is_some_and(|at| at > now_ms())
    }

    /// A code was sent for this view and has not expired.
    fn code_sent(&self, view_id: &str, company_id: i32) -> bool {
        self.str("pendingViewId") == Some(view_id)
            && self.int("pendingCompanyId") == Some(i64::from(company_id))
            && self.pending_live()
            && self.str("pendingCodeHash").is_some()
    }

    /// Adds the cookie to a response when the state changed.
    fn respond(&self, state: &AppState, response: impl IntoResponse) -> Response {
        let mut response = response.into_response();
        if self.changed {
            let value = encode_session_map(&self.data, &state.session_secret);
            if let Ok(cookie) = HeaderValue::from_str(&state.cookie.set_cookie(
                COOKIE,
                &value,
                VERIFIED_MAX_AGE_SECONDS,
            )) {
                response.headers_mut().append(header::SET_COOKIE, cookie);
            }
        }
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        response
    }
}

/// `a***@example.com`.
pub fn mask_email(email: &str) -> String {
    let email = email.trim();
    match email.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => {
            format!("{}***@{domain}", local.chars().next().unwrap_or_default())
        }
        _ => "your email".into(),
    }
}

/// The address in `Name <a@b>` or a bare address, lowercased.
fn parse_email(value: &str) -> String {
    let value = value.trim();
    let address = value
        .rfind('<')
        .and_then(|start| {
            value[start + 1..]
                .find('>')
                .map(|end| &value[start + 1..start + 1 + end])
        })
        .unwrap_or(value);
    address.trim().to_lowercase()
}

fn valid_email(email: &str) -> bool {
    email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !email.chars().any(char::is_whitespace)
    })
}

fn code_hash(secret: &str, view_id: &str, code: &str) -> String {
    Sha256::digest(format!("{secret}:{view_id}:{}", code.trim()).as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn new_code() -> ApiResult<String> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|_| ApiError::Internal("no randomness".into()))?;
    Ok(format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000))
}

/// May a code go out now (per visitor and per view)?
fn can_send(state: &AppState, visitor: &Visitor, key: &str) -> bool {
    let now = now_ms();
    let ready = visitor
        .int("pendingLastSentAt")
        .is_none_or(|at| now - at >= SEND_COOLDOWN_MS);
    let until = state
        .customer_view_cooldown
        .lock()
        .expect("cooldown lock")
        .get(key)
        .copied()
        .unwrap_or_default();
    ready && now >= until
}

fn mark_sent(state: &AppState, key: &str, at: i64) {
    state
        .customer_view_cooldown
        .lock()
        .expect("cooldown lock")
        .insert(key.to_string(), at + SEND_COOLDOWN_MS);
}

fn rate_limited() -> ApiError {
    ApiError::TooManyRequests("Please wait a moment before requesting another code.".into())
}

/// The customer whose page this is.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ViewCustomer {
    id: i32,
    name: Option<String>,
    email: Option<String>,
    view_id: String,
}

async fn by_view_id(state: &AppState, company_id: i32, view_id: &str) -> ApiResult<ViewCustomer> {
    if uuid::Uuid::parse_str(view_id).is_err() {
        return Err(ApiError::NotFound);
    }
    let row: Option<ViewCustomer> = sqlx::query_as(
        "SELECT c.id, c.name, ce.email, BIN_TO_UUID(c.view_id) AS view_id
           FROM customers c
           LEFT JOIN customers_emails ce ON ce.id = c.email_id AND ce.customer_id = c.id
          WHERE c.view_id = UUID_TO_BIN(?) AND c.company_id = ? AND c.deleted_at IS NULL
          LIMIT 1",
    )
    .bind(view_id)
    .bind(company_id)
    .fetch_optional(&state.pool)
    .await?;
    row.map(|mut c| {
        c.email = c
            .email
            .as_deref()
            .map(parse_email)
            .filter(|e| !e.is_empty());
        c
    })
    .ok_or(ApiError::NotFound)
}

async fn by_email(
    state: &AppState,
    company_id: i32,
    email: &str,
) -> ApiResult<Option<ViewCustomer>> {
    let row: Option<ViewCustomer> = sqlx::query_as(
        "SELECT c.id, c.name, ce.email, BIN_TO_UUID(c.view_id) AS view_id
           FROM customers c
           JOIN customers_emails ce ON ce.customer_id = c.id
          WHERE c.company_id = ? AND c.deleted_at IS NULL AND LOWER(ce.email) = ? AND c.view_id IS NOT NULL
          LIMIT 1",
    )
    .bind(company_id)
    .bind(email)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.map(|mut c| {
        c.email = Some(
            c.email
                .as_deref()
                .map(parse_email)
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| email.to_string()),
        );
        c
    }))
}

fn code_email_html(code: &str) -> String {
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Your login code</h2>
  <p style="color: #475569; line-height: 1.6;">Use this code to open your customer page. It expires in 10 minutes.</p>
  <p style="font-size: 28px; letter-spacing: 6px; font-weight: 700; color: #0f172a; margin: 24px 0;">{code}</p>
  <p style="color: #94a3b8; font-size: 13px;">If you did not request this, you can ignore this email.</p>
</div>"#
    )
}

/// Emails a code for `customer` and records it as pending (`sendCustomerViewLoginCode`).
async fn send_code(
    state: &AppState,
    visitor: &mut Visitor,
    company_id: i32,
    customer: &ViewCustomer,
) -> ApiResult<()> {
    let Some(email) = customer.email.clone() else {
        return Err(ApiError::Conflict(
            "This page is locked. Ask the company to add your email so we can send a login code."
                .into(),
        ));
    };
    let key = format!("{company_id}:{}", customer.view_id);
    if !can_send(state, visitor, &key) {
        return Err(rate_limited());
    }
    let code = new_code()?;
    let message = OutgoingEmail {
        from_name: None,
        from_address: "noreply@granite-manager.com".into(),
        to: vec![email.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Your customer login code".into(),
        body: code_email_html(&code),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(error) = state.webhooks.send_email(&message).await {
        tracing::warn!(?error, company_id, "customer view code email failed");
        return Err(ApiError::Upstream(
            "We could not send the email. Please try again.".into(),
        ));
    }
    let now = now_ms();
    mark_sent(state, &key, now);
    visitor.unset("pendingDummy");
    visitor.set("pendingViewId", json!(customer.view_id));
    visitor.set("pendingCompanyId", json!(company_id));
    visitor.set("pendingEmail", json!(email));
    visitor.set(
        "pendingCodeHash",
        json!(code_hash(&state.session_secret, &customer.view_id, &code)),
    );
    visitor.set("pendingExpiresAt", json!(now + CODE_EXPIRY_MS));
    visitor.set("pendingAttempts", json!(0));
    visitor.set("pendingLastSentAt", json!(now));
    Ok(())
}

/// Checks a code against the pending one (`verifyCustomerViewLoginCode`);
/// on success the visitor is verified for `view_id`.
fn verify(
    state: &AppState,
    visitor: &mut Visitor,
    view_id: &str,
    company_id: i32,
    code: &str,
) -> ApiResult<()> {
    let invalid = || ApiError::BadRequest("That code is not valid.".into());
    let attempts = visitor.int("pendingAttempts").unwrap_or_default();
    if visitor
        .int("pendingExpiresAt")
        .is_some_and(|at| at <= now_ms())
    {
        return Err(ApiError::BadRequest(
            "That code expired. Request a new one.".into(),
        ));
    }
    if attempts >= MAX_VERIFY_ATTEMPTS {
        return Err(ApiError::BadRequest(
            "Too many attempts. Request a new code.".into(),
        ));
    }
    if visitor.data.get("pendingDummy") == Some(&Value::Bool(true)) {
        if visitor.int("pendingCompanyId") == Some(i64::from(company_id)) {
            visitor.set("pendingAttempts", json!(attempts + 1));
        }
        return Err(invalid());
    }
    let Some(expected) = visitor.str("pendingCodeHash").map(str::to_string) else {
        return Err(invalid());
    };
    if view_id.is_empty()
        || visitor.str("pendingViewId") != Some(view_id)
        || visitor.int("pendingCompanyId") != Some(i64::from(company_id))
        || visitor.int("pendingExpiresAt").is_none()
    {
        return Err(invalid());
    }
    let normalized: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let actual = code_hash(&state.session_secret, view_id, &normalized);
    let matches = normalized.len() == 6
        && normalized.bytes().all(|b| b.is_ascii_digit())
        && actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if !matches {
        visitor.set("pendingAttempts", json!(attempts + 1));
        return Err(invalid());
    }
    visitor.clear_pending();
    visitor.set("verifiedViewId", json!(view_id));
    visitor.set("verifiedCompanyId", json!(company_id));
    visitor.set("verifiedAt", json!(now_ms()));
    Ok(())
}

/// The response for `result`, with the visitor's cookie either way (failed
/// attempts are counted).
fn finish<T: IntoResponse>(state: &AppState, visitor: &Visitor, result: ApiResult<T>) -> Response {
    match result {
        Ok(ok) => visitor.respond(state, ok),
        Err(error) => visitor.respond(state, error),
    }
}

/// Whether the visitor may see the page, and where a code would go.
#[derive(Debug, Serialize, ToSchema)]
pub struct ViewGate {
    /// The visitor entered a code for this page (valid for 7 days).
    pub verified: bool,
    /// The customer's name, once verified.
    pub customer_name: Option<String>,
    /// The customer has an email a code can go to.
    pub has_email: bool,
    /// That email, masked (`a***@example.com`).
    pub masked_email: Option<String>,
    /// A code was sent and has not expired.
    pub code_sent: bool,
}

/// Open a customer's page
///
/// Whether the visitor already entered a code for it, else where a code
/// would go. Unknown pages are 404.
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}",
    operation_id = "get_customer_view",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The gate", body = ViewGate),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn view_gate(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> Response {
    let visitor = Visitor::read(&state, &headers);
    let result = async {
        let customer = by_view_id(&state, company_id, &view_id).await?;
        let verified = visitor.verified(&view_id, company_id);
        Ok(Json(ViewGate {
            verified,
            customer_name: verified.then(|| customer.name.clone().unwrap_or_default()),
            has_email: customer.email.is_some(),
            masked_email: customer.email.as_deref().map(mask_email),
            code_sent: visitor.code_sent(&view_id, company_id),
        }))
    }
    .await;
    finish(&state, &visitor, result)
}

/// Email a login code
///
/// Sends a one-time code (10 minutes) to the customer's email.
#[utoipa::path(
    post,
    path = "/v1/public/customer-view/{company_id}/{view_id}/code",
    operation_id = "send_customer_view_code",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 204, description = "Sent"),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 409, description = "The customer has no email", body = ErrorBody),
        (status = 429, description = "A code was just sent", body = ErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
    )
)]
pub async fn send_view_code(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> Response {
    let mut visitor = Visitor::read(&state, &headers);
    let result = async {
        let customer = by_view_id(&state, company_id, &view_id).await?;
        send_code(&state, &mut visitor, company_id, &customer).await?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    finish(&state, &visitor, result)
}

/// A code the visitor typed.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EnteredCode {
    /// The 6 digits from the email.
    pub code: String,
}

/// Enter a login code
///
/// Unlocks the page for this browser for 7 days.
#[utoipa::path(
    post,
    path = "/v1/public/customer-view/{company_id}/{view_id}/verify",
    operation_id = "verify_customer_view_code",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    request_body = EnteredCode,
    responses(
        (status = 204, description = "Verified"),
        (status = 400, description = "Wrong or expired code, or too many attempts", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
    )
)]
pub async fn verify_view_code(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
    Json(input): Json<EnteredCode>,
) -> Response {
    let mut visitor = Visitor::read(&state, &headers);
    let result = async {
        by_view_id(&state, company_id, &view_id).await?;
        verify(&state, &mut visitor, &view_id, company_id, &input.code)?;
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    finish(&state, &visitor, result)
}

/// The verified customer of a page.
async fn verified_customer(
    state: &AppState,
    headers: &HeaderMap,
    company_id: i32,
    view_id: &str,
) -> ApiResult<ViewCustomer> {
    let customer = by_view_id(state, company_id, view_id).await?;
    if !Visitor::read(state, headers).verified(view_id, company_id) {
        return Err(ApiError::Unauthenticated("Login required".into()));
    }
    Ok(customer)
}

/// One of the customer's projects (a sale).
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct ViewProject {
    /// Sale id.
    pub id: i32,
    /// When it was sold (ISO 8601, UTC).
    pub sale_date: Option<String>,
    /// Price.
    pub price: Option<f64>,
    /// The sales rep.
    pub seller_name: Option<String>,
    /// The stone.
    pub stone_name: Option<String>,
    /// The stone's pictures, comma-separated URLs.
    pub stone_pictures: Option<String>,
    /// The sink.
    pub sink: Option<String>,
    /// `Sold`, `Partially Cut`, `Cut`, `Installed` or `Cancelled`.
    pub status: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    #[sqlx(flatten)]
    project: ViewProject,
    all_cut: Option<i64>,
    any_cut: Option<i64>,
}

/// List the customer's projects
///
/// Newest first, with the stone, sink and how far the work is. Needs a
/// verified visitor.
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/projects",
    operation_id = "list_customer_view_projects",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The projects", body = [ViewProject]),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn projects(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let rows: Vec<ProjectRow> = sqlx::query_as(
        "SELECT s.id,
                DATE_FORMAT(s.sale_date, '%Y-%m-%dT%H:%i:%sZ') AS sale_date,
                CAST(s.price AS DOUBLE) AS price,
                u.name AS seller_name,
                MAX(st.name) AS stone_name,
                GROUP_CONCAT(DISTINCT st.url ORDER BY si.id SEPARATOR ',') AS stone_pictures,
                MAX(sty.name) AS sink,
                CAST(MIN(CASE WHEN si.cut_date IS NULL THEN 0 ELSE 1 END) AS SIGNED) AS all_cut,
                CAST(MAX(CASE WHEN si.cut_date IS NOT NULL THEN 1 ELSE 0 END) AS SIGNED) AS any_cut,
                CASE
                  WHEN s.cancelled_date IS NOT NULL THEN 'Cancelled'
                  WHEN s.installed_date IS NOT NULL THEN 'Installed'
                  WHEN s.sale_date IS NOT NULL THEN 'Sold'
                END AS status
           FROM sales s
           JOIN customers c ON s.customer_id = c.id
           LEFT JOIN users u ON s.seller_id = u.id
           LEFT JOIN slab_inventory si ON si.sale_id = s.id
           LEFT JOIN stones st ON st.id = si.stone_id
           LEFT JOIN sinks sk ON sk.slab_id = si.id AND sk.is_deleted = 0
           LEFT JOIN sink_type sty ON sty.id = sk.sink_type_id
          WHERE c.id = ? AND c.company_id = ?
          GROUP BY s.id, s.sale_date, s.price, u.name
          ORDER BY s.sale_date DESC",
    )
    .bind(customer.id)
    .bind(company_id)
    .fetch_all(&state.pool)
    .await?;
    let projects: Vec<ViewProject> = rows
        .into_iter()
        .map(|row| {
            let mut project = row.project;
            if project.status.as_deref() == Some("Sold") {
                if row.all_cut == Some(1) {
                    project.status = Some("Cut".into());
                } else if row.any_cut == Some(1) {
                    project.status = Some("Partially Cut".into());
                }
            }
            project
        })
        .collect();
    Ok(no_store(Json(projects)))
}

/// List the customer's online quotes
///
/// The quotes the customer made with the online quote, newest first: each
/// with its drawing and each color's total (no price breakdown). Only for a
/// verified visitor.
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/quotes",
    operation_id = "list_customer_view_quotes",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The quotes", body = [crate::routes::online_quotes::CustomerOnlineQuote]),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn quotes(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let quotes =
        crate::routes::online_quotes::customer_online_quotes(&state.pool, company_id, customer.id)
            .await?;
    Ok(no_store(Json(quotes)))
}

fn no_store(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// List the customer's invoices
///
/// Their `QuickBooks` invoices, newest first; `status` says why there are
/// none. Needs a verified visitor.
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/invoices",
    operation_id = "list_customer_view_invoices",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
    ),
    responses(
        (status = 200, description = "The invoices", body = CustomerInvoices),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 404, description = "No such page", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn invoices(
    State(state): State<AppState>,
    Path((company_id, view_id)): Path<(i32, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    let found = invoices_of_customer(&state, company_id, customer.id).await?;
    Ok(no_store(Json(found)))
}

/// Get one of the customer's invoices as a PDF
///
/// Only an invoice of this customer's `QuickBooks` customer. Needs a
/// verified visitor.
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/{view_id}/invoices/{invoice_id}/pdf",
    operation_id = "get_customer_view_invoice_pdf",
    tag = "customer-view",
    params(
        ("company_id" = i32, Path, description = "Company id"),
        ("view_id" = String, Path, description = "The customer's page id (a UUID)"),
        ("invoice_id" = String, Path, description = "QuickBooks invoice id"),
    ),
    responses(
        (status = 200, description = "The PDF", content_type = "application/pdf", body = Vec<u8>),
        (status = 400, description = "Invalid invoice id", body = ErrorBody),
        (status = 401, description = "No code entered for this page", body = ErrorBody),
        (status = 403, description = "Not this customer's invoice", body = ErrorBody),
        (status = 404, description = "No such page, or not linked to QuickBooks", body = ErrorBody),
        (status = 409, description = "QuickBooks is not connected", body = ErrorBody),
        (status = 502, description = "QuickBooks failed", body = ErrorBody),
    )
)]
pub async fn invoice_pdf(
    State(state): State<AppState>,
    Path((company_id, view_id, invoice_id)): Path<(i32, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let customer = verified_customer(&state, &headers, company_id, &view_id).await?;
    customer_invoice_pdf_of(&state, company_id, customer.id, invoice_id.trim()).await
}

/// A sign-in by email in progress.
#[derive(Debug, Serialize, ToSchema)]
pub struct PendingLogin {
    /// A code was sent (or would have been) and has not expired.
    pub code_sent: bool,
    /// Where it went, masked.
    pub masked_email: Option<String>,
}

/// Check a pending sign-in
///
/// Whether a code was asked for on the company's account page
/// (`pendingAccountLoginChallenge`).
#[utoipa::path(
    get,
    path = "/v1/public/customer-view/{company_id}/login",
    operation_id = "get_customer_view_login",
    tag = "customer-view",
    params(("company_id" = i32, Path, description = "Company id")),
    responses((status = 200, description = "The pending sign-in", body = PendingLogin))
)]
pub async fn pending_login(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    headers: HeaderMap,
) -> Response {
    let visitor = Visitor::read(&state, &headers);
    let email = visitor.str("pendingEmail");
    let sent = visitor.int("pendingCompanyId") == Some(i64::from(company_id))
        && email.is_some()
        && visitor.pending_live()
        && (visitor.data.get("pendingDummy") == Some(&Value::Bool(true))
            || (visitor.str("pendingCodeHash").is_some()
                && visitor.str("pendingViewId").is_some()));
    visitor.respond(
        &state,
        Json(PendingLogin {
            code_sent: sent,
            masked_email: sent.then(|| mask_email(email.unwrap_or_default())),
        }),
    )
}

/// An address to sign in with.
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginEmail {
    /// The email on the customer's account; empty resends to the pending one.
    #[serde(default)]
    pub email: String,
}

/// Email a code to an account's address
///
/// Sends a code when a customer of the company has that email. The answer
/// is the same either way, so it does not reveal who is a customer.
#[utoipa::path(
    post,
    path = "/v1/public/customer-view/{company_id}/login/code",
    operation_id = "send_customer_view_login_code",
    tag = "customer-view",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = LoginEmail,
    responses(
        (status = 204, description = "Sent (if the address is a customer's)"),
        (status = 400, description = "Not an email", body = ErrorBody),
        (status = 429, description = "A code was just sent", body = ErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
    )
)]
pub async fn send_login_code(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    headers: HeaderMap,
    Json(input): Json<LoginEmail>,
) -> Response {
    let mut visitor = Visitor::read(&state, &headers);
    let result = async {
        let typed = parse_email(&input.email);
        let email = if typed.is_empty() {
            visitor
                .str("pendingEmail")
                .unwrap_or_default()
                .to_lowercase()
        } else {
            typed
        };
        if !valid_email(&email) {
            return Err(ApiError::BadRequest("Email is required".into()));
        }
        if let Some(customer) = by_email(&state, company_id, &email).await? {
            send_code(&state, &mut visitor, company_id, &customer).await?;
        } else {
            // No such customer: behave as if a code went out.
            let key = format!("{company_id}:dummy:{email}");
            if !can_send(&state, &visitor, &key) {
                return Err(rate_limited());
            }
            let now = now_ms();
            mark_sent(&state, &key, now);
            visitor.unset("pendingViewId");
            visitor.unset("pendingCodeHash");
            visitor.set("pendingDummy", Value::Bool(true));
            visitor.set("pendingCompanyId", json!(company_id));
            visitor.set("pendingEmail", json!(email));
            visitor.set("pendingExpiresAt", json!(now + CODE_EXPIRY_MS));
            visitor.set("pendingAttempts", json!(0));
            visitor.set("pendingLastSentAt", json!(now));
        }
        Ok(StatusCode::NO_CONTENT)
    }
    .await;
    finish(&state, &visitor, result)
}

/// The page a verified sign-in opens.
#[derive(Debug, Serialize, ToSchema)]
pub struct OpenedView {
    /// The customer's page id: open `/customer/{company_id}/{view_id}`.
    pub view_id: String,
}

/// Enter an account sign-in code
///
/// Verifies the code sent to the account's address; answers the page it
/// unlocks.
#[utoipa::path(
    post,
    path = "/v1/public/customer-view/{company_id}/login/verify",
    operation_id = "verify_customer_view_login_code",
    tag = "customer-view",
    params(("company_id" = i32, Path, description = "Company id")),
    request_body = EnteredCode,
    responses(
        (status = 200, description = "Verified", body = OpenedView),
        (status = 400, description = "Wrong or expired code, or too many attempts", body = ErrorBody),
    )
)]
pub async fn verify_login_code(
    State(state): State<AppState>,
    Path(company_id): Path<i32>,
    headers: HeaderMap,
    Json(input): Json<EnteredCode>,
) -> Response {
    let mut visitor = Visitor::read(&state, &headers);
    let view_id = visitor.str("pendingViewId").unwrap_or_default().to_string();
    let result = verify(&state, &mut visitor, &view_id, company_id, &input.code)
        .map(|()| Json(OpenedView { view_id }));
    finish(&state, &visitor, result)
}

/// Drop a pending code
///
/// So the visitor can sign in with another email.
#[utoipa::path(
    delete,
    path = "/v1/public/customer-view/login",
    operation_id = "cancel_customer_view_code",
    tag = "customer-view",
    responses((status = 204, description = "Dropped"))
)]
pub async fn cancel_code(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let mut visitor = Visitor::read(&state, &headers);
    visitor.clear_pending();
    visitor.respond(&state, StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_masked_and_parsed() {
        assert_eq!(mask_email("alice@example.com"), "a***@example.com");
        assert_eq!(mask_email("broken"), "your email");
        assert_eq!(
            parse_email("Alice <Alice@Example.com>"),
            "alice@example.com"
        );
        assert!(valid_email("a@b.co"));
        assert!(!valid_email("a@b"));
    }

    #[test]
    fn codes_hash_like_the_crm() {
        // sha256("secret:view:123456")
        assert_eq!(
            code_hash("secret", "view", " 123456 "),
            code_hash("secret", "view", "123456")
        );
        assert_eq!(code_hash("s", "v", "1").len(), 64);
    }
}
