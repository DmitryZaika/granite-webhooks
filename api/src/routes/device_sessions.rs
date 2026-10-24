//! Granite Phone sign-in: the app's device sessions.
//!
//! - `POST   /v1/auth/device-sessions`         → sign a phone in with the CRM email and password
//! - `DELETE /v1/auth/device-sessions/current` → sign this phone out (device token)
//! - `GET    /v1/me/devices`                   → the caller's signed-in phones (device or cookie)
//! - `DELETE /v1/me/devices/{id}`              → sign one of them out (device or cookie)
//!
//! Sign-in shares `POST /v1/auth/login`'s email normalization, constant-time
//! password check and failure limits (`login_attempts`). The device token is
//! shown once; only its SHA-256 is stored (`auth/device.rs`). So is the call
//! key the phone signs its calls with, which is derived and never stored.
//! Signing out goes through `phone::revoke` (Telnyx credential, push token).

use crate::auth::device::{
    DeviceUser, PhoneCaller, SESSION_MONTHS, encoded_call_key, new_token, token_hash,
};
use crate::auth::session::{SessionUser, effective_user, find_user, user_positions};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::revoke::{RevokeReason, revoke_device_session};
use crate::routes::auth::{
    client_ip, normalized_credentials, record_failed_login, sha256_hex, too_many_failures,
    verify_login,
};
use crate::serde_helpers::{js_date, js_date_required};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Months, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(sign_in))
        .routes(routes!(sign_out))
        .routes(routes!(list_devices))
        .routes(routes!(revoke_device))
}

/// Longest device name kept.
const DEVICE_NAME_CHARS: usize = 100;
/// Longest app version kept.
const APP_VERSION_CHARS: usize = 32;

/// The phone's operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Android,
    Ios,
}

impl Platform {
    /// As stored (`device_sessions.platform`, `phone_devices.platform`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }
}

/// A stored platform that is neither `android` nor `ios`.
#[derive(Debug, thiserror::Error)]
#[error("unknown platform")]
pub struct UnknownPlatform;

impl TryFrom<String> for Platform {
    type Error = UnknownPlatform;

    fn try_from(value: String) -> Result<Self, UnknownPlatform> {
        match value.as_str() {
            "android" => Ok(Self::Android),
            "ios" => Ok(Self::Ios),
            _ => Err(UnknownPlatform),
        }
    }
}

/// What a signed-in phone is: `android` or `ios` (the Granite Phone app),
/// or `web` (a browser, signed in by the CRM's phone dock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DevicePlatform {
    Android,
    Ios,
    Web,
}

impl TryFrom<String> for DevicePlatform {
    type Error = UnknownPlatform;

    fn try_from(value: String) -> Result<Self, UnknownPlatform> {
        match value.as_str() {
            "android" => Ok(Self::Android),
            "ios" => Ok(Self::Ios),
            "web" => Ok(Self::Web),
            _ => Err(UnknownPlatform),
        }
    }
}

/// Body of `POST /v1/auth/device-sessions`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSignIn {
    /// The CRM login email (any case, spaces around it ignored).
    pub email: String,
    /// The CRM password.
    pub password: String,
    /// Shown in "Signed-in devices", e.g. "Rita's Pixel 9"; cut to 100
    /// characters.
    pub device_name: String,
    /// `android` or `ios`.
    pub platform: Platform,
    /// The app's version, e.g. `1.4.0`; cut to 32 characters.
    pub app_version: String,
}

impl std::fmt::Debug for DeviceSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceSignIn")
            .field("platform", &self.platform)
            .field("app_version", &self.app_version)
            .finish_non_exhaustive()
    }
}

/// The signed-in user, as the phone app shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PhoneUser {
    /// User id.
    pub id: i32,
    /// Display name; empty when the user has none.
    pub name: String,
    /// Login email.
    pub email: String,
    /// The user's company (every phone request acts in it).
    pub company_id: i32,
    /// That company's name.
    pub company_name: String,
    /// Company admin (or superuser / super admin): sees the admin screens.
    pub is_admin: bool,
    /// The portal a position locks the user into on the web, if any.
    ///
    /// The same rule as `GET /v1/app/shell` `portal` (`installer`,
    /// `check-in`, `marketing`, `shop_worker`, `template`); null for everyone
    /// else. For `installer` and `template` the app shows only the user's own
    /// schedule (`GET /v1/me/schedule`) and does not set the phone up for
    /// calls.
    pub portal: Option<String>,
}

/// The [`PhoneUser`] of a signed-in user.
pub async fn phone_user(pool: &MySqlPool, user: &SessionUser) -> sqlx::Result<PhoneUser> {
    let company_name: Option<String> = sqlx::query_scalar("SELECT name FROM company WHERE id = ?")
        .bind(user.company_id)
        .fetch_optional(pool)
        .await?;
    let positions = user_positions(pool, user.id).await?;
    Ok(PhoneUser {
        portal: crate::routes::app_shell::user_portal(user, &positions),
        id: user.id,
        name: user.name.clone().unwrap_or_default(),
        email: user.email.clone(),
        company_id: user.company_id,
        company_name: company_name.unwrap_or_default(),
        is_admin: user.is_admin || user.is_superuser,
    })
}

/// `POST /v1/auth/device-sessions`: a phone just signed in.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSession {
    /// The device token: send it as the `X-Granite-Device` header on every
    /// request. Shown only this once (43 characters, base64url).
    pub token: String,
    /// The phone's call key: it signs every call the app places (the
    /// `X-Granite-Call-Auth` SIP header, an HMAC-SHA256 keyed with these 32
    /// bytes; see the Telnyx `outbound` webhook). Shown only this once (43
    /// characters, base64url of 32 bytes). The server derives it from
    /// `SESSION_SECRET` and the session id and stores nothing.
    pub call_key: String,
    /// The device session (`SignedInDevice.id`).
    pub session_id: i64,
    /// When the sign-in ends at the latest: 12 months from now (sooner when
    /// the phone is unused for 30 days, signs out or is revoked).
    #[serde(serialize_with = "js_date_required")]
    pub expires_at: DateTime<Utc>,
    /// Who signed in.
    pub user: PhoneUser,
}

/// A phone signed in as the caller.
#[derive(Debug, Serialize, FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SignedInDevice {
    /// The device session: pass it to `DELETE /v1/me/devices/{id}`.
    pub id: i64,
    /// The name the phone gave at sign-in (a browser phone: e.g. "Chrome on
    /// Windows").
    pub device_name: String,
    /// `android` or `ios` (the app), or `web` (a browser phone of the CRM).
    #[sqlx(try_from = "String")]
    pub platform: DevicePlatform,
    /// The app version at sign-in; null when it sent none (always for a
    /// browser phone).
    pub app_version: Option<String>,
    /// When it signed in.
    #[serde(serialize_with = "js_date_required")]
    pub created_at: DateTime<Utc>,
    /// When it was last used (updated at most every 5 minutes); null when
    /// never since sign-in.
    #[serde(serialize_with = "js_date")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The caller's own phone (the device token of this request).
    pub current: bool,
}

/// `GET /v1/me/devices`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SignedInDevices {
    /// The caller's phones that are not signed out, newest first.
    pub devices: Vec<SignedInDevice>,
}

/// `{"error": token}` with the status, for the tokens the app reads.
fn refuse(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: error.into(),
        }),
    )
        .into_response()
}

/// At most `max` characters of `text`.
fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The user who signed in, as a session would load them; `None` when they
/// (or their company) are gone.
async fn signed_in_user(pool: &MySqlPool, user_id: i32) -> sqlx::Result<Option<SessionUser>> {
    let company_id: Option<i32> = sqlx::query_scalar(
        "SELECT u.company_id FROM users u
           JOIN company co ON co.id = u.company_id AND co.deleted_at IS NULL
          WHERE u.id = ? AND u.is_deleted = 0",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(company_id) = company_id else {
        return Ok(None);
    };
    let Some(user) = find_user(pool, user_id, company_id).await? else {
        return Ok(None);
    };
    let positions = user_positions(pool, user.id).await?;
    Ok(Some(effective_user(user, &positions, None)))
}

/// Sign a phone in
///
/// Signs the Granite Phone app in with the CRM email and password and
/// returns its device token (send it as the `X-Granite-Device` header) and
/// its call key (signs the calls it places), both shown only here. Only
/// employees, admins and superusers of a company that is not deleted may:
/// others get 403 `no_phone_access`.
/// Failed attempts count together with `POST /v1/auth/login`'s (10 per
/// email and address, 50 per email in 15 minutes, then 429
/// `rate_limited`), and a wrong email and a wrong password both answer 401
/// `invalid_credentials`. The sign-in lasts 12 months, or until the phone
/// is unused for 30 days, signs out or is revoked. Next:
/// `POST /v1/phone/devices` registers the phone for calling.
#[utoipa::path(
    post,
    path = "/v1/auth/device-sessions",
    operation_id = "create_device_session",
    tag = "phone",
    request_body = DeviceSignIn,
    responses(
        (status = 201, description = "Signed in; the device token and call key are in the body", body = DeviceSession),
        (status = 400, description = "A field is missing or empty, or the platform is unknown", body = ErrorBody),
        (status = 401, description = "`invalid_credentials`: wrong email or password", body = ErrorBody),
        (status = 403, description = "`no_phone_access`: not an employee, admin or superuser, or the company is deleted", body = ErrorBody),
        (status = 429, description = "`rate_limited`: too many failed attempts; try later", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    )
)]
pub async fn sign_in(
    State(state): State<AppState>,
    headers: HeaderMap,
    JsonBody(input): JsonBody<DeviceSignIn>,
) -> ApiResult<Response> {
    let Some((email, password)) = normalized_credentials(&input.email, &input.password) else {
        return Err(ApiError::BadRequest(
            "Please check your email and password.".into(),
        ));
    };
    let device_name = clip(input.device_name.trim(), DEVICE_NAME_CHARS);
    if device_name.is_empty() {
        return Err(ApiError::BadRequest("deviceName is required".into()));
    }
    let app_version =
        Some(clip(input.app_version.trim(), APP_VERSION_CHARS)).filter(|v| !v.is_empty());
    let pool = &state.pool;
    let email_hash = sha256_hex(&email);
    let ip_hash = sha256_hex(&client_ip(&headers));
    if too_many_failures(pool, &email_hash, &ip_hash).await? {
        return Ok(refuse(StatusCode::TOO_MANY_REQUESTS, "rate_limited"));
    }
    let Some(user_id) = verify_login(pool, &email, &password).await? else {
        record_failed_login(pool, &email_hash, &ip_hash).await?;
        return Ok(refuse(StatusCode::UNAUTHORIZED, "invalid_credentials"));
    };
    let Some(user) = signed_in_user(pool, user_id)
        .await?
        .filter(|user| user.is_employee || user.is_admin || user.is_superuser)
    else {
        return Ok(refuse(StatusCode::FORBIDDEN, "no_phone_access"));
    };
    let body_user = phone_user(pool, &user).await?;
    let token = new_token().map_err(|err| ApiError::Internal(err.to_string()))?;
    let expires_at = Utc::now()
        .trunc_subsecs(0)
        .checked_add_months(Months::new(SESSION_MONTHS))
        .ok_or_else(|| ApiError::Internal("no date a year from now".into()))?;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM login_attempts WHERE email_hash = ?")
        .bind(&email_hash)
        .execute(&mut *tx)
        .await?;
    let session_id = sqlx::query(
        "INSERT INTO device_sessions
           (user_id, company_id, token_hash, device_name, platform, app_version, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(token_hash(&token))
    .bind(&device_name)
    .bind(input.platform.as_str())
    .bind(app_version)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    tx.commit().await?;
    let session_id = i64::try_from(session_id)
        .map_err(|_| ApiError::Internal("device session id out of range".into()))?;
    let body = DeviceSession {
        token,
        call_key: encoded_call_key(&state.session_secret, session_id),
        session_id,
        expires_at,
        user: body_user,
    };
    Ok((
        StatusCode::CREATED,
        [(CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response())
}

/// Sign this phone out
///
/// Ends the device session of the request's `X-Granite-Device` token: the
/// token stops working, the phone's Telnyx credential is deleted and its
/// push token cleared. The app then wipes its local data. Device token only.
#[utoipa::path(
    delete,
    path = "/v1/auth/device-sessions/current",
    operation_id = "delete_current_device_session",
    tag = "phone",
    responses(
        (status = 204, description = "Signed out"),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn sign_out(State(state): State<AppState>, device: DeviceUser) -> ApiResult<StatusCode> {
    revoke_device_session(&state, device.session_id, RevokeReason::SignedOut).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List my signed-in phones
///
/// The caller's Granite Phone sign-ins that are not signed out, newest
/// first; `current` marks the phone making the request (always false with
/// the CRM's cookie). Revoke one with `DELETE /v1/me/devices/{id}`. Takes
/// the device token or the session cookie (employees, admins, superusers).
#[utoipa::path(
    get,
    path = "/v1/me/devices",
    operation_id = "list_my_devices",
    tag = "phone",
    responses(
        (status = 200, description = "The caller's phones", body = SignedInDevices),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "Cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn list_devices(
    State(state): State<AppState>,
    caller: PhoneCaller,
) -> ApiResult<Json<SignedInDevices>> {
    let devices = sqlx::query_as(
        "SELECT CAST(id AS SIGNED) AS id, device_name, CAST(platform AS CHAR) AS platform,
                app_version, created_at, last_seen_at, (id <=> ?) AS current
           FROM device_sessions
          WHERE user_id = ? AND company_id = ? AND revoked_at IS NULL
          ORDER BY created_at DESC, id DESC",
    )
    .bind(caller.session_id())
    .bind(caller.user.id)
    .bind(caller.user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(SignedInDevices { devices }))
}

/// Sign one of my phones out
///
/// Revokes one of the caller's device sessions (an id from
/// `GET /v1/me/devices`; it may be the calling phone itself): its token stops
/// working, its Telnyx credential is deleted and the phone is told to wipe
/// itself. 404 when the session is not the caller's or is signed out already.
/// Takes the device token or the session cookie.
#[utoipa::path(
    delete,
    path = "/v1/me/devices/{id}",
    operation_id = "revoke_my_device",
    tag = "phone",
    params(("id" = i64, Path, description = "Device session id (`SignedInDevice.id`)")),
    responses(
        (status = 204, description = "Signed out"),
        (status = 401, description = "No valid device token or session", body = ErrorBody),
        (status = 403, description = "Cookie of a user who is not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not one of the caller's signed-in phones", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []), ("session_cookie" = []))
)]
pub async fn revoke_device(
    State(state): State<AppState>,
    caller: PhoneCaller,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let mine: Option<i64> = sqlx::query_scalar(
        "SELECT CAST(id AS SIGNED) FROM device_sessions
          WHERE id = ? AND user_id = ? AND company_id = ? AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(caller.user.id)
    .bind(caller.user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    if mine.is_none() || !revoke_device_session(&state, id, RevokeReason::ByUser).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
