//! Sign-in and the session cookie.
//!
//! - `POST /v1/auth/login`                       → check the password, set the session cookie
//! - `POST /v1/auth/logout`                      → end the session, clear the cookie
//! - `GET  /v1/auth/session`                     → who is signed in and where they start
//! - `PUT  /v1/auth/active-company`              → a super admin acts in another company
//! - `POST /v1/auth/password-reset`              → email a reset link
//! - `GET  /v1/auth/password-reset/{token}`      → is the link still good
//! - `POST /v1/auth/password-reset/{token}`      → set a new password
//! - `GET  /v1/auth/invite/{token}`              → whose invite it is
//! - `POST /v1/auth/invite/{token}`              → set up the account, sign in
//!
//! Replaces the Remix `login`, `logout`, `forgot-password`,
//! `reset-password.$token` and `api.superadmin.switchCompany` routes. The
//! cookie keeps the Remix wire format (`auth/cookie.rs`), so sessions carry
//! over in both directions while pages move.
//!
//! Security: passwords are checked with bcrypt off the async runtime, and an
//! unknown email costs the same work as a wrong password. Failed sign-ins
//! are counted in the database (Lambda instances share no memory): at most
//! 10 per email and address and 50 per email in 15 minutes. Reset links
//! are 384-bit random tokens stored as SHA-256, valid for an hour and once,
//! built from `APP_URL` (never from request headers); asking for one says
//! the same whether or not the email exists. Setting a password ends every
//! session of the user. Invite links (a manager added the user by email) are
//! the same tokens, valid for 7 days and once, and only while the account
//! has no password; using one signs in.

use crate::app_origin::{app_origin, company_subdomain};
use crate::auth::cookie::{
    SESSION_COOKIE_NAME, SESSION_MAX_AGE_SECONDS, SessionData, cookie_values, decode_session_map,
    encode_session, encode_session_map, request_session_id,
};
use crate::auth::session::{SUPER_ADMIN_POSITION, SessionUser, find_session_user, user_positions};
use crate::auth::{CurrentUser, sha256_hex};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use common::utils::email_body::escape_html;
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::fmt::Write;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(session))
        .routes(routes!(set_active_company))
        .routes(routes!(request_reset))
        .routes(routes!(check_reset, reset_password))
        .routes(routes!(check_invite, accept_invite))
}

/// Failed sign-ins allowed per email and client address in the window.
const MAX_FAILURES_PER_ADDRESS: i64 = 10;
/// Failed sign-ins allowed per email from anywhere in the window.
const MAX_FAILURES_PER_EMAIL: i64 = 50;
/// The rate limit window.
const WINDOW_MINUTES: i64 = 15;
/// Reset tokens: 48 random bytes, hex.
const RESET_TOKEN_BYTES: usize = 48;
/// A reset link works this long.
const RESET_MINUTES: i64 = 60;
/// One reset email per user this often.
const RESET_INTERVAL_SECONDS: i64 = 60;
/// An invite link works this long.
const INVITE_DAYS: i64 = 7;
/// Shortest new password.
const MIN_PASSWORD: usize = 8;
/// bcrypt cost of new passwords (as Remix).
const BCRYPT_COST: u32 = 10;
/// Checked when the email is unknown, so it takes as long as a real check.
static DUMMY_HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    bcrypt::hash("granite-manager-dummy", BCRYPT_COST).unwrap_or_default()
});

/// Positions that lock a user into one portal, first match wins
/// (`PORTAL_POSITIONS` in `portalLock.server.ts`).
const INSTALLER: i32 = 6;
const CHECK_IN: i32 = 8;
const MARKETING: i32 = 7;
const SHOP_WORKER: i32 = 4;
const TEMPLATE_MAKER: i32 = 11;

fn random_token() -> Result<String, ApiError> {
    let mut bytes = [0u8; RESET_TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(96), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        }))
}

/// The client address (in `api_core::auth`, which self-serve signup uses too).
pub use crate::auth::{ClientIp, client_ip};

/// The verified session cookie of the request, as every key it holds.
fn request_session(
    headers: &HeaderMap,
    secret: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| cookie_values(header, SESSION_COOKIE_NAME))
        .find_map(|value| decode_session_map(&value, secret))
}

pub(crate) fn with_cookies(mut response: Response, cookies: Vec<String>) -> Response {
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().append(SET_COOKIE, value);
        }
    }
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

/// Where a user starts after signing in (`getPostLoginRedirectPath`):
/// their portal when a position locks them into one, else `/employee`.
fn home_path(user: &SessionUser, positions: &[i32]) -> String {
    if user.is_superuser || positions.contains(&SUPER_ADMIN_POSITION) {
        return "/employee".into();
    }
    let company = user.company_id;
    for position in [INSTALLER, CHECK_IN, MARKETING, SHOP_WORKER, TEMPLATE_MAKER] {
        if positions.contains(&position) {
            return match position {
                INSTALLER => format!("/installers/{company}/checklist"),
                CHECK_IN => format!("/customer/{company}/check-in"),
                MARKETING => format!("/external/marketing/{company}/leads"),
                SHOP_WORKER => "/shop/transactions".into(),
                _ => format!("/templates/{company}/schedule"),
            };
        }
    }
    "/employee".into()
}

/// A company the super admin may act in.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct SwitchableCompany {
    /// Company id.
    pub id: i32,
    /// Name.
    pub name: String,
}

/// `GET /v1/auth/session` and the login result.
#[derive(Debug, Serialize, ToSchema)]
pub struct AuthSession {
    /// The signed-in user (the company is the one requests act in).
    pub user: SessionUser,
    /// Where the user starts: their portal, or `/employee`.
    pub home_path: String,
    /// The subdomain of the acting company, for its own host
    /// (`{subdomain}.granite-manager.com`); null when it has none.
    pub company_subdomain: Option<String>,
    /// Companies a super admin can switch to (empty for everyone else).
    pub switchable_companies: Vec<SwitchableCompany>,
}

async fn auth_session(pool: &sqlx::MySqlPool, user: SessionUser) -> ApiResult<AuthSession> {
    let positions: Vec<i32> = user_positions(pool, user.id)
        .await?
        .iter()
        .map(|row| row.position_id)
        .collect();
    let switchable_companies = if positions.contains(&SUPER_ADMIN_POSITION) {
        sqlx::query_as(
            "SELECT c.id, c.name FROM company c
               JOIN users_positions up ON up.company_id = c.id
              WHERE up.user_id = ? AND up.position_id = ? AND c.deleted_at IS NULL
              ORDER BY c.name, c.id",
        )
        .bind(user.id)
        .bind(SUPER_ADMIN_POSITION)
        .fetch_all(pool)
        .await?
    } else {
        Vec::new()
    };
    Ok(AuthSession {
        home_path: home_path(&user, &positions),
        company_subdomain: company_subdomain(pool, user.company_id).await?,
        switchable_companies,
        user,
    })
}

/// Body of `POST /v1/auth/login`.
#[derive(Deserialize, ToSchema)]
pub struct LoginInput {
    /// Login email (any case).
    pub email: String,
    /// Password.
    pub password: String,
}

impl std::fmt::Debug for LoginInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginInput")
            .field("email", &self.email)
            .finish_non_exhaustive()
    }
}

/// Whether bcrypt accepts the password for the hash, off the async runtime.
async fn password_matches(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}

pub async fn too_many_failures(
    pool: &sqlx::MySqlPool,
    email_hash: &str,
    ip_hash: &str,
) -> sqlx::Result<bool> {
    let (per_address, per_email): (i64, i64) = sqlx::query_as(
        "SELECT CAST(COALESCE(SUM(ip_hash = ?), 0) AS SIGNED), COUNT(*)
           FROM login_attempts
          WHERE email_hash = ? AND attempted_at > NOW() - INTERVAL ? MINUTE",
    )
    .bind(ip_hash)
    .bind(email_hash)
    .bind(WINDOW_MINUTES)
    .fetch_one(pool)
    .await?;
    Ok(per_address >= MAX_FAILURES_PER_ADDRESS || per_email >= MAX_FAILURES_PER_EMAIL)
}

/// A sign-in's email and password as they are checked (the email trimmed
/// and lowercased, the password trimmed); `None` when either is empty or
/// too long.
pub fn normalized_credentials(email: &str, password: &str) -> Option<(String, String)> {
    let email = email.trim().to_lowercase();
    let password = password.trim().to_string();
    if email.is_empty() || password.is_empty() || email.len() > 254 || password.len() > 200 {
        return None;
    }
    Some((email, password))
}

/// The live user `password` is right for, among every live account with
/// the (normalized) `email`. An unknown email costs a check against
/// `DUMMY_HASH`, so timing does not tell emails apart.
pub async fn verify_login(
    pool: &sqlx::MySqlPool,
    email: &str,
    password: &str,
) -> sqlx::Result<Option<i32>> {
    let candidates: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, password FROM users WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 ORDER BY id",
    )
    .bind(email)
    .fetch_all(pool)
    .await?;
    let mut checked = false;
    for (id, hash) in candidates {
        let Some(hash) = hash.filter(|hash| !hash.is_empty()) else {
            continue;
        };
        checked = true;
        if password_matches(password.to_string(), hash).await {
            return Ok(Some(id));
        }
    }
    if !checked {
        // Same cost as a real check, so timing does not tell emails apart.
        let _ = password_matches(password.to_string(), DUMMY_HASH.clone()).await;
    }
    Ok(None)
}

/// Counts a failed sign-in for the rate limits, and prunes day-old ones.
pub async fn record_failed_login(
    pool: &sqlx::MySqlPool,
    email_hash: &str,
    ip_hash: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO login_attempts (email_hash, ip_hash) VALUES (?, ?)")
        .bind(email_hash)
        .bind(ip_hash)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM login_attempts WHERE attempted_at < NOW() - INTERVAL 1 DAY LIMIT 500")
        .execute(pool)
        .await?;
    Ok(())
}

/// Sign in
///
/// Checks the email and password; on success creates a session (60 days),
/// sets it as the `__session` cookie (HttpOnly, SameSite=Lax, Secure in
/// production) and says where the user starts. Too many failed attempts
/// for the email answer 429 until the 15-minute window passes. A wrong
/// email and a wrong password answer the same 401.
#[crate::openapi::path(
    post,
    path = "/v1/auth/login",
    operation_id = "sign_in",
    tag = "auth",
    request_body = LoginInput,
    responses(
        (status = 200, description = "Signed in; the session cookie is set", body = AuthSession),
        (status = 400, description = "Email or password missing", body = ErrorBody),
        (status = 401, description = "Incorrect email or password", body = ErrorBody),
        (status = 429, description = "Too many failed attempts", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    JsonBody(input): JsonBody<LoginInput>,
) -> ApiResult<Response> {
    let Some((email, password)) = normalized_credentials(&input.email, &input.password) else {
        return Err(ApiError::BadRequest(
            "Please check your email and password.".into(),
        ));
    };
    let pool = &state.pool;
    let email_hash = sha256_hex(&email);
    let ip_hash = sha256_hex(&ip);
    if too_many_failures(pool, &email_hash, &ip_hash).await? {
        return Err(ApiError::TooManyRequests(
            "Too many login attempts. Please try again later.".into(),
        ));
    }
    let Some(user_id) = verify_login(pool, &email, &password).await? else {
        record_failed_login(pool, &email_hash, &ip_hash).await?;
        return Err(ApiError::Unauthenticated(
            "Incorrect email or password. Please try again.".into(),
        ));
    };
    sqlx::query("DELETE FROM login_attempts WHERE email_hash = ?")
        .bind(&email_hash)
        .execute(pool)
        .await?;
    let started = start_session(&state, user_id).await?;
    let body = auth_session(pool, started.user).await?;
    Ok(with_cookies(Json(body).into_response(), started.cookies))
}

/// A new session and the `Set-Cookie` values that carry it.
pub(crate) struct StartedSession {
    pub user: SessionUser,
    pub cookies: Vec<String>,
}

/// Signs `user_id` in: a 60-day `sessions` row and its `__session` cookie
/// (put it on the response with [`with_cookies`]).
pub(crate) async fn start_session(state: &AppState, user_id: i32) -> ApiResult<StartedSession> {
    let pool = &state.pool;
    let session_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expiration_date)
         VALUES (?, ?, NOW() + INTERVAL ? SECOND)",
    )
    .bind(&session_id)
    .bind(user_id)
    .bind(SESSION_MAX_AGE_SECONDS)
    .execute(pool)
    .await?;
    let user = find_session_user(pool, &session_id)
        .await?
        .ok_or_else(|| ApiError::Internal("Failed to create session. Please try again.".into()))?;
    let cookie = encode_session(
        &SessionData {
            session_id: Some(session_id),
            active_company_id: None,
        },
        &state.session_secret,
    );
    Ok(StartedSession {
        user,
        cookies: state.cookie.set_session(&cookie),
    })
}

/// Sign out
///
/// Ends the request's session (it no longer signs anyone in, even if the
/// cookie is replayed) and clears the cookie. The browser phone of the
/// session (Granite Phone in the CRM) is signed out too: its Telnyx
/// credential is deleted. Always answers 204.
#[crate::openapi::path(
    post,
    path = "/v1/auth/logout",
    operation_id = "sign_out",
    tag = "auth",
    responses(
        (status = 204, description = "Signed out; the cookie is cleared"),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(session_id) = request_session_id(&headers, &state.session_secret) {
        sqlx::query("UPDATE sessions SET is_deleted = 1 WHERE id = ?")
            .bind(&session_id)
            .execute(&state.pool)
            .await?;
        crate::phone::revoke::revoke_web_session(
            &state,
            &session_id,
            crate::phone::revoke::RevokeReason::SignedOut,
        )
        .await?;
    }
    Ok(with_cookies(
        StatusCode::NO_CONTENT.into_response(),
        state.cookie.clear_session(),
    ))
}

/// The current session
///
/// The signed-in user (for a super admin, in the company they act in), the
/// path they start at, the acting company's subdomain and the companies a
/// super admin can switch to. 401 when signed out.
#[crate::openapi::path(
    get,
    path = "/v1/auth/session",
    operation_id = "get_auth_session",
    tag = "auth",
    responses(
        (status = 200, description = "Signed in", body = AuthSession),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Response> {
    let body = auth_session(&state.pool, user).await?;
    let mut response = Json(body).into_response();
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Body of `PUT /v1/auth/active-company`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ActiveCompany {
    /// A company the caller holds the Super Admin position in.
    pub company_id: i32,
}

/// Act in another company
///
/// A super admin picks which of their companies requests act in; the
/// choice is kept in the session cookie (its other contents stay).
#[crate::openapi::path(
    put,
    path = "/v1/auth/active-company",
    operation_id = "set_active_company",
    tag = "auth",
    request_body = ActiveCompany,
    responses(
        (status = 200, description = "Switched; the cookie is updated", body = AuthSession),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not a super admin of that company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_active_company(
    State(state): State<AppState>,
    headers: HeaderMap,
    CurrentUser(user): CurrentUser,
    JsonBody(input): JsonBody<ActiveCompany>,
) -> ApiResult<Response> {
    let allowed: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users_positions up JOIN company c ON c.id = up.company_id
          WHERE up.user_id = ? AND up.position_id = ? AND up.company_id = ? AND c.deleted_at IS NULL
          LIMIT 1",
    )
    .bind(user.id)
    .bind(SUPER_ADMIN_POSITION)
    .bind(input.company_id)
    .fetch_optional(&state.pool)
    .await?;
    if allowed.is_none() {
        return Err(ApiError::Forbidden);
    }
    let mut session =
        request_session(&headers, &state.session_secret).ok_or(ApiError::Unauthorized)?;
    session.insert("activeCompanyId".into(), input.company_id.into());
    let cookie = encode_session_map(&session, &state.session_secret);
    let body = auth_session(
        &state.pool,
        SessionUser {
            company_id: input.company_id,
            ..user
        },
    )
    .await?;
    Ok(with_cookies(
        Json(body).into_response(),
        state.cookie.set_session(&cookie),
    ))
}

/// Body of `POST /v1/auth/password-reset`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResetRequest {
    /// The account's email.
    pub email: String,
}

/// Email a password reset link
///
/// When the email belongs to an account (and none was sent to it in the last
/// minute), stores a new one-hour link (older ones stop working) and emails
/// it. Always answers 202 the same way, so it does not tell which emails
/// have accounts.
#[crate::openapi::path(
    post,
    path = "/v1/auth/password-reset",
    operation_id = "request_password_reset",
    tag = "auth",
    request_body = ResetRequest,
    responses(
        (status = 202, description = "If the account exists, a link is on its way"),
        (status = 400, description = "Not an email address", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
        (status = 502, description = "The email could not be sent", body = ErrorBody),
    )
)]
pub async fn request_reset(
    State(state): State<AppState>,
    JsonBody(input): JsonBody<ResetRequest>,
) -> ApiResult<StatusCode> {
    let email = input.email.trim().to_lowercase();
    if email.len() > 254
        || !email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
    {
        return Err(ApiError::BadRequest(
            "Please enter a valid email address.".into(),
        ));
    }
    let pool = &state.pool;
    let user: Option<(i32, String, i32)> = sqlx::query_as(
        "SELECT id, email, company_id FROM users
          WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 ORDER BY id LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(pool)
    .await?;
    let Some((user_id, address, company_id)) = user else {
        return Ok(StatusCode::ACCEPTED);
    };
    let recent: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM password_reset_tokens
          WHERE user_id = ? AND created_at > NOW() - INTERVAL ? SECOND LIMIT 1",
    )
    .bind(user_id)
    .bind(RESET_INTERVAL_SECONDS)
    .fetch_optional(pool)
    .await?;
    if recent.is_some() {
        return Ok(StatusCode::ACCEPTED);
    }
    let token = random_token()?;
    let token_hash = sha256_hex(&token);
    sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE user_id = ? AND used = FALSE")
        .bind(user_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
         VALUES (?, ?, NOW() + INTERVAL ? MINUTE)",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(RESET_MINUTES)
    .execute(pool)
    .await?;
    let subdomain = company_subdomain(pool, company_id).await?;
    let url = format!(
        "{}/reset-password/{token}",
        app_origin(&state.app_url, subdomain.as_deref())
    );
    let email = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: "noreply@granite-manager.com".into(),
        to: vec![address.trim().to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Reset your password".into(),
        body: reset_email_html(&url),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    if let Err(err) = state.webhooks.send_email(&email).await {
        tracing::warn!(?err, user_id, "password reset email failed");
        sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE token_hash = ?")
            .bind(&token_hash)
            .execute(pool)
            .await?;
        return Err(ApiError::Upstream(
            "Failed to send email. Please try again later.".into(),
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

fn reset_email_html(url: &str) -> String {
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Reset Your Password</h2>
  <p style="color: #475569; line-height: 1.6;">You requested a password reset. Click the button below to set a new password. This link expires in {RESET_MINUTES} minutes.</p>
  <a href="{url}" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Reset Password</a>
  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">If you didn't request this, you can safely ignore this email.</p>
  <p style="color: #94a3b8; font-size: 12px;">Link: {url}</p>
</div>"#
    )
}

/// The user of a live, unused reset token.
async fn reset_user(pool: &sqlx::MySqlPool, token: &str) -> sqlx::Result<Option<i32>> {
    if token.len() != RESET_TOKEN_BYTES * 2 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    sqlx::query_scalar(
        "SELECT user_id FROM password_reset_tokens
          WHERE token_hash = ? AND used = FALSE AND expires_at > NOW() LIMIT 1",
    )
    .bind(sha256_hex(token))
    .fetch_optional(pool)
    .await
}

/// Check a reset link
///
/// 204 while the link can still set a password, 404 when it is unknown,
/// used or expired.
#[crate::openapi::path(
    get,
    path = "/v1/auth/password-reset/{token}",
    operation_id = "check_password_reset",
    tag = "auth",
    params(("token" = String, Path, description = "The token from the emailed link")),
    responses(
        (status = 204, description = "The link works"),
        (status = 404, description = "Invalid or expired link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn check_reset(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<StatusCode> {
    reset_user(&state.pool, &token)
        .await?
        .map(|_| StatusCode::NO_CONTENT)
        .ok_or(ApiError::NotFound)
}

/// Body of `POST /v1/auth/password-reset/{token}`.
#[derive(Deserialize, ToSchema)]
pub struct NewPassword {
    /// At least 8 characters.
    pub password: String,
}

impl std::fmt::Debug for NewPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewPassword").finish_non_exhaustive()
    }
}

/// Set a new password
///
/// Uses a reset link once: sets the password (bcrypt), spends the link and
/// ends every session of the user, Granite Phone sign-ins included.
#[crate::openapi::path(
    post,
    path = "/v1/auth/password-reset/{token}",
    operation_id = "reset_password",
    tag = "auth",
    params(("token" = String, Path, description = "The token from the emailed link")),
    request_body = NewPassword,
    responses(
        (status = 204, description = "Password set; sign in again"),
        (status = 400, description = "Password too short", body = ErrorBody),
        (status = 404, description = "Invalid or expired link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn reset_password(
    State(state): State<AppState>,
    Path(token): Path<String>,
    JsonBody(input): JsonBody<NewPassword>,
) -> ApiResult<StatusCode> {
    // Trimmed like sign-in does, so the new password always signs in.
    let password = input.password.trim().to_string();
    if password.chars().count() < MIN_PASSWORD || password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let user_id = reset_user(&state.pool, &token)
        .await?
        .ok_or(ApiError::NotFound)?;
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    // Spend the link first: a second use of the same token finds nothing.
    let spent = sqlx::query(
        "UPDATE password_reset_tokens SET used = TRUE
          WHERE token_hash = ? AND used = FALSE AND expires_at > NOW()",
    )
    .bind(sha256_hex(&token))
    .execute(&mut *tx)
    .await?;
    if spent.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    sqlx::query("UPDATE users SET password = ? WHERE id = ?")
        .bind(hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET is_deleted = 1 WHERE user_id = ? AND is_deleted = 0")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::phone::revoke::revoke_user_phones(
        &state,
        user_id,
        crate::phone::revoke::RevokeReason::PasswordChanged,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An invite link that was made for a user.
#[derive(Debug, Serialize, ToSchema)]
pub struct SentInvite {
    /// The link, to share another way if the email does not arrive.
    pub invite_url: String,
    /// The link was emailed to the user.
    pub email_sent: bool,
}

/// Makes a new invite link for a user (older links stop working) and emails
/// it to them. A failed email still answers the link, for the manager to
/// share.
pub async fn send_invite(state: &AppState, user_id: i32) -> ApiResult<SentInvite> {
    let pool = &state.pool;
    let (address, company_id, company_name): (String, i32, Option<String>) = sqlx::query_as(
        "SELECT u.email, u.company_id, c.name FROM users u
           LEFT JOIN company c ON c.id = u.company_id
          WHERE u.id = ? AND u.is_deleted = 0",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    let token = random_token()?;
    sqlx::query("UPDATE password_reset_tokens SET used = TRUE WHERE user_id = ? AND used = FALSE")
        .bind(user_id)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
         VALUES (?, ?, NOW() + INTERVAL ? DAY)",
    )
    .bind(user_id)
    .bind(sha256_hex(&token))
    .bind(INVITE_DAYS)
    .execute(pool)
    .await?;
    let subdomain = company_subdomain(pool, company_id).await?;
    let invite_url = format!(
        "{}/invite/{token}",
        app_origin(&state.app_url, subdomain.as_deref())
    );
    let company = company_name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "your company".into());
    let email = OutgoingEmail {
        from_name: Some("Granite Manager".into()),
        from_address: "noreply@granite-manager.com".into(),
        to: vec![address.trim().to_string()],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: format!("You're invited to {company} on Granite Manager"),
        body: invite_email_html(&company, &invite_url),
        signature: None,
        reply_to: Vec::new(),
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
        transactional: true,
    };
    let email_sent = match state.webhooks.send_email(&email).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(?err, user_id, "invite email failed");
            false
        }
    };
    Ok(SentInvite {
        invite_url,
        email_sent,
    })
}

fn invite_email_html(company: &str, url: &str) -> String {
    let company = escape_html(company);
    format!(
        r#"<div style="font-family: Arial, sans-serif; max-width: 480px; margin: 0 auto;">
  <h2 style="color: #1e293b;">Join {company}</h2>
  <p style="color: #475569; line-height: 1.6;">You've been added to {company} on Granite Manager. Click the button below to enter your name, phone number and password. This link expires in {INVITE_DAYS} days.</p>
  <a href="{url}" style="display: inline-block; background-color: #2563eb; color: white; padding: 12px 24px; border-radius: 6px; text-decoration: none; font-weight: 600; margin: 16px 0;">Set up your account</a>
  <p style="color: #94a3b8; font-size: 13px; margin-top: 24px;">If you weren't expecting this, you can safely ignore this email.</p>
  <p style="color: #94a3b8; font-size: 12px;">Link: {url}</p>
</div>"#
    )
}

#[derive(sqlx::FromRow)]
struct InvitedUser {
    id: i32,
    email: String,
    company_name: Option<String>,
}

/// The user of a live, unused link who has no password yet.
async fn invited_user(pool: &sqlx::MySqlPool, token: &str) -> sqlx::Result<Option<InvitedUser>> {
    if token.len() != RESET_TOKEN_BYTES * 2 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    sqlx::query_as(
        "SELECT u.id, u.email, c.name AS company_name
           FROM password_reset_tokens t
           JOIN users u ON u.id = t.user_id
           LEFT JOIN company c ON c.id = u.company_id
          WHERE t.token_hash = ? AND t.used = FALSE AND t.expires_at > NOW()
            AND u.is_deleted = 0 AND (u.password IS NULL OR u.password = '')
          LIMIT 1",
    )
    .bind(sha256_hex(token))
    .fetch_optional(pool)
    .await
}

/// `GET /v1/auth/invite/{token}`.
#[derive(Debug, Serialize, ToSchema)]
pub struct InviteDetails {
    /// The account's email; they sign in with it.
    pub email: String,
    /// The company that added them.
    pub company_name: Option<String>,
}

/// Check an invite link
///
/// Whose invite it is while the link can still set up the account; 404 when
/// it is unknown, used or expired, or the account already has a password.
#[crate::openapi::path(
    get,
    path = "/v1/auth/invite/{token}",
    operation_id = "check_invite",
    tag = "auth",
    params(("token" = String, Path, description = "The token from the emailed link")),
    responses(
        (status = 200, description = "The link works", body = InviteDetails),
        (status = 404, description = "Invalid or expired link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn check_invite(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Json<InviteDetails>> {
    let invited = invited_user(&state.pool, &token)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(InviteDetails {
        email: invited.email,
        company_name: invited.company_name,
    }))
}

/// Body of `POST /v1/auth/invite/{token}`.
#[derive(Deserialize, ToSchema)]
pub struct JoinInput {
    /// Their name.
    pub name: String,
    /// Their phone, at least 10 digits.
    pub phone_number: String,
    /// At least 8 characters.
    pub password: String,
}

impl std::fmt::Debug for JoinInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinInput")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Set up an invited account
///
/// Uses an invite link once: saves the user's name, phone and password
/// (bcrypt), spends the link and signs in as `POST /v1/auth/login` does.
#[crate::openapi::path(
    post,
    path = "/v1/auth/invite/{token}",
    operation_id = "accept_invite",
    tag = "auth",
    params(("token" = String, Path, description = "The token from the emailed link")),
    request_body = JoinInput,
    responses(
        (status = 200, description = "Set up and signed in; the session cookie is set", body = AuthSession),
        (status = 400, description = "Name missing, phone or password too short", body = ErrorBody),
        (status = 404, description = "Invalid or expired link", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn accept_invite(
    State(state): State<AppState>,
    Path(token): Path<String>,
    JsonBody(input): JsonBody<JoinInput>,
) -> ApiResult<Response> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::BadRequest("Name is required".into()));
    }
    let phone = input.phone_number.trim().to_string();
    if phone.chars().filter(char::is_ascii_digit).count() < 10 {
        return Err(ApiError::BadRequest(
            "Phone must include at least 10 digits".into(),
        ));
    }
    // Trimmed like sign-in does, so the new password always signs in.
    let password = input.password.trim().to_string();
    if password.chars().count() < MIN_PASSWORD || password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let invited = invited_user(&state.pool, &token)
        .await?
        .ok_or(ApiError::NotFound)?;
    let hash = tokio::task::spawn_blocking(move || bcrypt::hash(password, BCRYPT_COST))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    let mut tx = state.pool.begin().await?;
    // Spend the link first: a second use of the same token finds nothing.
    let spent = sqlx::query(
        "UPDATE password_reset_tokens SET used = TRUE
          WHERE token_hash = ? AND used = FALSE AND expires_at > NOW()",
    )
    .bind(sha256_hex(&token))
    .execute(&mut *tx)
    .await?;
    if spent.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    let saved = sqlx::query(
        "UPDATE users SET name = ?, phone_number = ?, password = ?
          WHERE id = ? AND is_deleted = 0 AND (password IS NULL OR password = '')",
    )
    .bind(&name)
    .bind(&phone)
    .bind(hash)
    .bind(invited.id)
    .execute(&mut *tx)
    .await?;
    if saved.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    let started = start_session(&state, invited.id).await?;
    let body = auth_session(&state.pool, started.user).await?;
    Ok(with_cookies(Json(body).into_response(), started.cookies))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_random() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 96);
        assert_ne!(a, b);
        assert_eq!(sha256_hex("abc").len(), 64);
    }

    #[test]
    fn the_dummy_hash_is_a_bcrypt_hash() {
        assert!(bcrypt::verify("x", &DUMMY_HASH).is_ok());
    }
}
