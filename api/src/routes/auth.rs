//! Sign-in and the session cookie.
//!
//! - `POST /v1/auth/login`                       → check the password, set the session cookie
//! - `POST /v1/auth/logout`                      → end the session, clear the cookie
//! - `GET  /v1/auth/session`                     → who is signed in and where they start
//! - `PUT  /v1/auth/active-company`              → a super admin acts in another company
//! - `POST /v1/auth/password-reset`              → email a reset link
//! - `GET  /v1/auth/password-reset/{token}`      → is the link still good
//! - `POST /v1/auth/password-reset/{token}`      → set a new password
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
//! session of the user.

use crate::auth::CurrentUser;
use crate::auth::cookie::{
    SESSION_COOKIE_NAME, SESSION_MAX_AGE_SECONDS, SessionData, cookie_value, decode_session,
    decode_session_map, encode_session, encode_session_map,
};
use crate::auth::session::{SUPER_ADMIN_POSITION, SessionUser, find_session_user, user_positions};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use crate::webhooks_client::OutgoingEmail;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(session))
        .routes(routes!(set_active_company))
        .routes(routes!(request_reset))
        .routes(routes!(check_reset, reset_password))
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

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

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

/// The client address: the first `X-Forwarded-For` entry (API Gateway and
/// CloudFront set it), else "unknown".
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        })
        .unwrap_or("unknown")
        .to_string()
}

/// The verified session cookie of the request, as every key it holds.
fn request_session(
    headers: &HeaderMap,
    secret: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find_map(|header| cookie_value(header, SESSION_COOKIE_NAME))
        .and_then(|value| decode_session_map(&value, secret))
}

/// The request's session id, when its cookie verifies.
fn request_session_id(headers: &HeaderMap, secret: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .find_map(|header| cookie_value(header, SESSION_COOKIE_NAME))
        .and_then(|value| decode_session(&value, secret))
        .and_then(|session| session.session_id)
}

fn with_cookies(mut response: Response, cookies: Vec<String>) -> Response {
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
    for position in [INSTALLER, CHECK_IN, MARKETING, SHOP_WORKER] {
        if positions.contains(&position) {
            return match position {
                INSTALLER => format!("/installers/{company}/checklist"),
                CHECK_IN => format!("/customer/{company}/check-in"),
                MARKETING => format!("/external/marketing/{company}/leads"),
                _ => "/shop/transactions".into(),
            };
        }
    }
    "/employee".into()
}

async fn company_subdomain(
    pool: &sqlx::MySqlPool,
    company_id: i32,
) -> sqlx::Result<Option<String>> {
    let subdomain: Option<Option<String>> =
        sqlx::query_scalar("SELECT subdomain FROM company WHERE id = ? AND deleted_at IS NULL")
            .bind(company_id)
            .fetch_optional(pool)
            .await?;
    Ok(subdomain
        .flatten()
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty()))
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

async fn too_many_failures(
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

/// Sign in
///
/// Checks the email and password; on success creates a session (60 days),
/// sets it as the `__session` cookie (HttpOnly, SameSite=Lax, Secure in
/// production) and says where the user starts. Too many failed attempts
/// for the email answer 429 until the 15-minute window passes. A wrong
/// email and a wrong password answer the same 401.
#[utoipa::path(
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
    headers: HeaderMap,
    JsonBody(input): JsonBody<LoginInput>,
) -> ApiResult<Response> {
    let email = input.email.trim().to_lowercase();
    let password = input.password.trim().to_string();
    if email.is_empty() || password.is_empty() || email.len() > 254 || password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Please check your email and password.".into(),
        ));
    }
    let pool = &state.pool;
    let email_hash = sha256_hex(&email);
    let ip_hash = sha256_hex(&client_ip(&headers));
    if too_many_failures(pool, &email_hash, &ip_hash).await? {
        return Err(ApiError::TooManyRequests(
            "Too many login attempts. Please try again later.".into(),
        ));
    }
    let candidates: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, password FROM users WHERE LOWER(TRIM(email)) = ? AND is_deleted = 0 ORDER BY id",
    )
    .bind(&email)
    .fetch_all(pool)
    .await?;
    let mut matched = None;
    let mut checked = false;
    for (id, hash) in candidates {
        let Some(hash) = hash.filter(|hash| !hash.is_empty()) else {
            continue;
        };
        checked = true;
        if password_matches(password.clone(), hash).await {
            matched = Some(id);
            break;
        }
    }
    if !checked {
        // Same cost as a real check, so timing does not tell emails apart.
        let _ = password_matches(password, DUMMY_HASH.clone()).await;
    }
    let Some(user_id) = matched else {
        sqlx::query("INSERT INTO login_attempts (email_hash, ip_hash) VALUES (?, ?)")
            .bind(&email_hash)
            .bind(&ip_hash)
            .execute(pool)
            .await?;
        sqlx::query(
            "DELETE FROM login_attempts WHERE attempted_at < NOW() - INTERVAL 1 DAY LIMIT 500",
        )
        .execute(pool)
        .await?;
        return Err(ApiError::Unauthenticated(
            "Incorrect email or password. Please try again.".into(),
        ));
    };
    sqlx::query("DELETE FROM login_attempts WHERE email_hash = ?")
        .bind(&email_hash)
        .execute(pool)
        .await?;
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
    let body = auth_session(pool, user).await?;
    let cookie = encode_session(
        &SessionData {
            session_id: Some(session_id),
            active_company_id: None,
        },
        &state.session_secret,
    );
    Ok(with_cookies(
        Json(body).into_response(),
        state.cookie.set_session(&cookie),
    ))
}

/// Sign out
///
/// Ends the request's session (it no longer signs anyone in, even if the
/// cookie is replayed) and clears the cookie. Always answers 204.
#[utoipa::path(
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
            .bind(session_id)
            .execute(&state.pool)
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
#[utoipa::path(
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
#[utoipa::path(
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

/// The app origin for the user's company: `https://{subdomain}.{apex}` when
/// the company has a subdomain and the app is not on localhost, else `APP_URL`.
fn app_origin(app_url: &str, subdomain: Option<&str>) -> String {
    let Some(subdomain) = subdomain else {
        return app_url.to_string();
    };
    let Some((scheme, rest)) = app_url.split_once("://") else {
        return app_url.to_string();
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let bare = host.split(':').next().unwrap_or(host);
    if bare == "localhost"
        || bare.ends_with(".localhost")
        || bare.parse::<std::net::IpAddr>().is_ok()
    {
        return app_url.to_string();
    }
    let apex = host.strip_prefix("www.").unwrap_or(host);
    format!("{scheme}://{subdomain}.{apex}")
}

/// Email a password reset link
///
/// When the email belongs to an account (and none was sent to it in the last
/// minute), stores a new one-hour link (older ones stop working) and emails
/// it. Always answers 202 the same way, so it does not tell which emails
/// have accounts.
#[utoipa::path(
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
#[utoipa::path(
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
/// ends every session of the user.
#[utoipa::path(
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
    if input.password.chars().count() < MIN_PASSWORD || input.password.len() > 200 {
        return Err(ApiError::BadRequest(
            "Password must be at least 8 characters long.".into(),
        ));
    }
    let user_id = reset_user(&state.pool, &token)
        .await?
        .ok_or(ApiError::NotFound)?;
    let password = input.password;
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
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_links_use_the_company_host() {
        assert_eq!(
            app_origin("https://granite-manager.com", Some("acme")),
            "https://acme.granite-manager.com"
        );
        assert_eq!(
            app_origin("https://www.granite-manager.com", Some("acme")),
            "https://acme.granite-manager.com"
        );
        assert_eq!(
            app_origin("http://localhost:5173", Some("acme")),
            "http://localhost:5173"
        );
        assert_eq!(
            app_origin("https://granite-manager.com", None),
            "https://granite-manager.com"
        );
    }

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
        assert!(bcrypt::verify("x", &*DUMMY_HASH).is_ok());
    }
}
