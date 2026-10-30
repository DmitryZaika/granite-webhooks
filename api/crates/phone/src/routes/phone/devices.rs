//! This session's phone: registration (and its Telnyx credential), the
//! Telnyx login JWT, the FCM push token.

use super::{RateLimited, denied, rate_limited, telnyx_failed};
use crate::auth::device::DeviceUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::phone::company::{self, CompanyConfig};
use crate::phone::devices::{self, CREDENTIAL_DAYS, CredentialError, PhoneDevice};
use crate::phone::older_than;
use crate::routes::device_sessions::Platform;
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// Longest device name kept.
const DEVICE_NAME_CHARS: usize = 100;
/// Longest app version kept.
const APP_VERSION_CHARS: usize = 32;
/// The running app version, on every request from the app and the native module.
const APP_VERSION_HEADER: &str = "x-granite-app-version";
/// Longest FCM token accepted.
const PUSH_TOKEN_CHARS: usize = 512;
/// Login JWTs a phone may get: one a minute, six an hour.
const TOKEN_GAP_SECS: i64 = 60;
const TOKENS_PER_HOUR: i32 = 6;
/// How long a Telnyx login JWT works when it does not say.
pub(super) const JWT_HOURS: i64 = 24;

/// Body of `POST /v1/phone/devices`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRegistration {
    /// `android` or `ios`.
    pub platform: Platform,
    /// The app's version, e.g. `1.4.0` (cut to 32 characters; empty =
    /// unknown).
    pub app_version: String,
    /// Shown in "Signed-in devices", e.g. "Rita's Pixel 9" (cut to 100
    /// characters).
    pub device_name: String,
    /// The phone's FCM registration token, for our pushes; null when it has
    /// none yet (send it later with `PUT /v1/phone/devices/{id}/push-token`).
    #[serde(default)]
    pub push_token: Option<String>,
}

/// Body of `PUT /v1/phone/devices/{id}/push-token`.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushTokenInput {
    /// The phone's current FCM registration token (at most 512 characters).
    pub push_token: String,
}

/// `POST /v1/phone/devices/{id}/token`.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceToken {
    /// The Telnyx login JWT for this phone's SIP credential.
    pub jwt: String,
    /// When the JWT stops working (RFC 3339): about 24 hours from now.
    #[serde(serialize_with = "js_date_required")]
    pub expires_at: DateTime<Utc>,
}

/// At most `max` characters of `text`, trimmed.
pub(super) fn clip(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

pub(super) fn credential_failed(err: CredentialError) -> ApiError {
    match err {
        CredentialError::Telnyx(err) => telnyx_failed(&err, "credential"),
        CredentialError::Database(err) => ApiError::Database(err),
    }
}

/// Register this phone
///
/// Registers the phone of this device session for calling, or updates it
/// (idempotent: 201 the first time, 200 after). While calling is on for the
/// company it also gets its Telnyx telephony credential (working 30 days,
/// extended with every login JWT); `sipUsername` stays null while calling is
/// off. A `pushToken` here or later (`PUT .../push-token`) lets us push
/// missed calls; no other phone keeps that token. Device token only. Next:
/// `POST /v1/phone/devices/{id}/token`.
#[crate::openapi::path(
    post,
    path = "/v1/phone/devices",
    operation_id = "register_phone_device",
    tag = "phone",
    request_body = DeviceRegistration,
    responses(
        (status = 201, description = "Registered", body = PhoneDevice),
        (status = 200, description = "Already registered; updated", body = PhoneDevice),
        (status = 400, description = "Missing device name, unknown platform or too long a push token", body = ErrorBody),
        (status = 401, description = "No valid device token, or `device_revoked`", body = ErrorBody),
        (status = 502, description = "Telnyx did not create the credential; try again", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn register_device(
    State(state): State<AppState>,
    device: DeviceUser,
    JsonBody(input): JsonBody<DeviceRegistration>,
) -> ApiResult<(StatusCode, Json<PhoneDevice>)> {
    let device_name = clip(&input.device_name, DEVICE_NAME_CHARS);
    if device_name.is_empty() {
        return Err(ApiError::BadRequest("deviceName is required".into()));
    }
    let app_version = Some(clip(&input.app_version, APP_VERSION_CHARS)).filter(|v| !v.is_empty());
    let push_token = input
        .push_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty());
    if push_token.is_some_and(|token| token.chars().count() > PUSH_TOKEN_CHARS) {
        return Err(ApiError::BadRequest("pushToken is too long".into()));
    }
    let user = &device.user;
    let pool = &state.pool;
    // One statement, so two registrations of the session at once neither
    // deadlock nor make two phones; a revoked phone is left as it is.
    let created = device.device_id.is_none();
    sqlx::query(
        "INSERT INTO phone_devices
           (device_session_id, user_id, company_id, platform, device_name, app_version)
         VALUES (?, ?, ?, ?, ?, ?) AS new
         ON DUPLICATE KEY UPDATE
           platform = IF(phone_devices.revoked_at IS NULL, new.platform, phone_devices.platform),
           device_name = IF(phone_devices.revoked_at IS NULL, new.device_name, phone_devices.device_name),
           app_version = IF(phone_devices.revoked_at IS NULL, new.app_version, phone_devices.app_version)",
    )
    .bind(device.session_id)
    .bind(user.id)
    .bind(user.company_id)
    .bind(input.platform.as_str())
    .bind(&device_name)
    .bind(&app_version)
    .execute(pool)
    .await?;
    let (device_id, revoked): (i64, bool) = sqlx::query_as(
        "SELECT CAST(id AS SIGNED), revoked_at IS NOT NULL FROM phone_devices
          WHERE device_session_id = ?",
    )
    .bind(device.session_id)
    .fetch_one(pool)
    .await?;
    if revoked {
        return Err(ApiError::Unauthenticated("device_revoked".into()));
    }
    if let Some(token) = push_token {
        devices::set_push_token(pool, device_id, token).await?;
    }
    let config = company::load(pool, user.company_id).await?;
    devices::ensure_credential(&state, config.as_ref(), device_id)
        .await
        .map_err(credential_failed)?;
    let registered = devices::session_device(pool, device.session_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(registered)))
}

/// A device's login JWT counters before a mint, to give the slot back when
/// Telnyx fails.
pub(super) struct TokenSlot {
    last_token_at: Option<DateTime<Utc>>,
    window_start: Option<DateTime<Utc>>,
    window_count: i32,
}

/// Takes one of the device's JWT slots (one a minute, six an hour): the
/// counters before, or the seconds to wait.
pub(super) async fn take_token_slot(
    pool: &MySqlPool,
    device_id: i64,
) -> sqlx::Result<Result<TokenSlot, u64>> {
    let now = Utc::now().trunc_subsecs(0);
    let mut tx = pool.begin().await?;
    let (last_token_at, window_start, window_count): (
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        i32,
    ) = sqlx::query_as(
        "SELECT last_token_at, token_window_start, token_window_count FROM phone_devices
          WHERE id = ? FOR UPDATE",
    )
    .bind(device_id)
    .fetch_one(&mut *tx)
    .await?;
    let wait = |seconds: i64| Ok(Err(u64::try_from(seconds.max(1)).unwrap_or(1)));
    if let Some(last) = last_token_at {
        let since = (now - last).num_seconds();
        if since < TOKEN_GAP_SECS {
            tx.rollback().await?;
            return wait(TOKEN_GAP_SECS - since);
        }
    }
    let (start, count) = match window_start {
        Some(start) if (now - start) < Duration::hours(1) => (start, window_count),
        _ => (now, 0),
    };
    if count >= TOKENS_PER_HOUR {
        tx.rollback().await?;
        return wait((start + Duration::hours(1) - now).num_seconds());
    }
    sqlx::query(
        "UPDATE phone_devices SET last_token_at = ?, token_window_start = ?, token_window_count = ?
          WHERE id = ?",
    )
    .bind(now)
    .bind(start)
    .bind(count + 1)
    .bind(device_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Ok(TokenSlot {
        last_token_at,
        window_start,
        window_count,
    }))
}

/// Gives a JWT slot back (the mint failed).
pub(super) async fn give_back(pool: &MySqlPool, device_id: i64, slot: TokenSlot) {
    let restored = sqlx::query(
        "UPDATE phone_devices SET last_token_at = ?, token_window_start = ?, token_window_count = ?
          WHERE id = ?",
    )
    .bind(slot.last_token_at)
    .bind(slot.window_start)
    .bind(slot.window_count)
    .bind(device_id)
    .execute(pool)
    .await;
    if let Err(err) = restored {
        tracing::warn!(device_id, ?err, "token slot not given back");
    }
}

/// When a JWT expires, from its `exp` claim.
pub(super) fn jwt_expiry(jwt: &str) -> Option<DateTime<Utc>> {
    let payload = jwt.split('.').nth(1)?;
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    DateTime::from_timestamp(claims["exp"].as_i64()?, 0)
}

/// The app version the phone says it runs now (`X-Granite-App-Version`, sent
/// by the app and by the native calling module), clipped; None when absent.
pub(super) fn running_version(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(APP_VERSION_HEADER)?.to_str().ok()?;
    Some(clip(value, APP_VERSION_CHARS)).filter(|v| !v.is_empty())
}

/// Keeps the phone's stored app version current after an app update, so the
/// update check and the admin pages see the version it runs now.
pub(super) async fn save_running_version(
    pool: &MySqlPool,
    device_id: i64,
    version: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE phone_devices SET app_version = ?
          WHERE id = ? AND revoked_at IS NULL AND NOT (app_version <=> ?)",
    )
    .bind(version)
    .bind(device_id)
    .bind(version)
    .execute(pool)
    .await?;
    Ok(())
}

/// The phone's app version (as registered, else at sign-in).
async fn app_version(pool: &MySqlPool, device_id: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(pd.app_version, ''), ds.app_version) FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE pd.id = ?",
    )
    .bind(device_id)
    .fetch_one(pool)
    .await
}

/// Get a Telnyx login JWT
///
/// Mints a Telnyx login JWT (about 24 hours) for this phone's SIP credential
/// and extends the credential to 30 days; makes the credential first when
/// the phone has none yet. At most one a minute and six an hour per phone
/// (429 with `retryAfterSecs`); a failed mint does not count. 403
/// `calling_disabled` while the company's calling is off, 403
/// `update_required` when the app is older than the minimum version (the
/// version in `X-Granite-App-Version` when sent, which is also saved; else
/// the registered one), 404 when `id` is not this session's phone. The
/// native calling module calls it too, with the same headers.
#[crate::openapi::path(
    post,
    path = "/v1/phone/devices/{id}/token",
    operation_id = "mint_phone_device_token",
    tag = "phone",
    params(("id" = i64, Path, description = "This session's phone device (`PhoneDevice.id`)")),
    responses(
        (status = 200, description = "The JWT", body = DeviceToken),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 403, description = "`calling_disabled` or `update_required`", body = ErrorBody),
        (status = 404, description = "Not this session's phone", body = ErrorBody),
        (status = 429, description = "`rate_limited`: one a minute, six an hour", body = RateLimited),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn mint_device_token(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if device.device_id != Some(id) {
        return Err(ApiError::NotFound);
    }
    let pool = &state.pool;
    if let Some(running) = running_version(&headers) {
        save_running_version(pool, id, &running).await?;
    }
    let config: CompanyConfig = company::load(pool, device.user.company_id)
        .await?
        .filter(|config| config.calling_on() && config.credential_connection_id.is_some())
        .ok_or_else(|| denied("calling_disabled"))?;
    if let (Some(minimum), Some(version)) = (
        config.min_app_version(&state.phone),
        app_version(pool, id).await?,
    ) && older_than(&version, &minimum)
    {
        return Err(denied("update_required"));
    }
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Calling is not set up on the server.".into(),
        ));
    }
    let slot = match take_token_slot(pool, id).await? {
        Ok(slot) => slot,
        Err(wait) => return Ok(rate_limited(wait)),
    };
    let credential = match devices::ensure_credential(&state, Some(&config), id).await {
        Ok(Some((credential_id, _))) => credential_id,
        Ok(None) => {
            give_back(pool, id, slot).await;
            return Err(denied("calling_disabled"));
        }
        Err(err) => {
            give_back(pool, id, slot).await;
            return Err(credential_failed(err));
        }
    };
    let extended = Utc::now() + Duration::days(CREDENTIAL_DAYS);
    match state
        .telnyx
        .set_credential_expiry(&credential, extended)
        .await
    {
        Ok(()) => {
            sqlx::query("UPDATE phone_devices SET credential_expires_at = ? WHERE id = ?")
                .bind(extended)
                .bind(id)
                .execute(pool)
                .await?;
        }
        Err(err) => tracing::warn!(device_id = id, error = %err, "credential not extended"),
    }
    let jwt = match state.telnyx.mint_token(&credential).await {
        Ok(jwt) => jwt,
        Err(err) => {
            give_back(pool, id, slot).await;
            return Err(telnyx_failed(&err, "login token"));
        }
    };
    let expires_at = jwt_expiry(&jwt).unwrap_or_else(|| Utc::now() + Duration::hours(JWT_HOURS));
    Ok(Json(DeviceToken { jwt, expires_at }).into_response())
}

/// Set this phone's push token
///
/// Saves the phone's FCM registration token (after it changes), for our
/// pushes: missed calls, voicemails, AI messages, new emails, sign-outs. No
/// other phone keeps the token. 404 when `id` is not this session's phone. The native
/// calling module calls it too.
#[crate::openapi::path(
    put,
    path = "/v1/phone/devices/{id}/push-token",
    operation_id = "set_phone_push_token",
    tag = "phone",
    params(("id" = i64, Path, description = "This session's phone device (`PhoneDevice.id`)")),
    request_body = PushTokenInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Empty or too long", body = ErrorBody),
        (status = 401, description = "No valid device token", body = ErrorBody),
        (status = 404, description = "Not this session's phone", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("device_token" = []))
)]
pub async fn set_device_push_token(
    State(state): State<AppState>,
    device: DeviceUser,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<PushTokenInput>,
) -> ApiResult<StatusCode> {
    if device.device_id != Some(id) {
        return Err(ApiError::NotFound);
    }
    let token = input.push_token.trim();
    if token.is_empty() || token.chars().count() > PUSH_TOKEN_CHARS {
        return Err(ApiError::BadRequest(
            "pushToken must be 1 to 512 characters".into(),
        ));
    }
    devices::set_push_token(&state.pool, id, token).await?;
    Ok(StatusCode::NO_CONTENT)
}
