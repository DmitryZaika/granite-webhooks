//! The CRM's phone dock: the browser as a Granite Phone, signed in with the
//! web session cookie.
//!
//! A browser phone is a device session of platform `web` whose
//! `web_session_id` is the cookie's session, with its phone device and
//! Telnyx credential: one per web session (tabs share it). It rings with the
//! user's app phones while its tab is connected to Telnyx (`presence`), and
//! its calls are approved like the app's: the browser signs
//! `X-Granite-Call-Auth` with its call key, handed out at registration. Its
//! device token is random and never handed out, so the app's device-token
//! routes cannot be used with it. Signing the web session out (or its end),
//! a password change or the user's removal signs it out like an app phone
//! (`phone::revoke`), and so does `DELETE /v1/me/devices/{id}`.

use super::devices::{
    DeviceToken, JWT_HOURS, clip, credential_failed, give_back, jwt_expiry, take_token_slot,
};
use super::{RateLimited, denied, rate_limited, telnyx_failed};
use crate::auth::EmployeeUser;
use crate::auth::device::{encoded_call_key, new_token, token_hash};
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::phone::availability::Availability;
use crate::phone::company::{self, CompanyConfig, RecordingMode};
use crate::phone::devices::{self, CREDENTIAL_DAYS, LIVE_DEVICE};
use crate::phone::lines::{self, CallerId, default_caller_id};
use crate::phone::revoke::{RevokeReason, revoke_device_session};
use crate::phone::settings::{self, PhoneSettings};
use crate::routes::auth::request_session_id;
use crate::serde_helpers::js_date_required;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, MySqlPool};
use utoipa::ToSchema;

/// A browser phone rings this long after its tab last said it is connected
/// (the dock says so every minute).
pub const ONLINE_SECS: i64 = 150;
/// Longest device name kept.
const DEVICE_NAME_CHARS: usize = 100;
/// `last_seen_at` is written at most this often.
const SEEN_EVERY_SECS: i64 = 300;

/// `GET /v1/phone/web/bootstrap`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebPhoneBootstrap {
    /// Our clock now (RFC 3339): the browser signs its calls with our time,
    /// so a wrong computer clock does not get them refused.
    #[serde(serialize_with = "js_date_required")]
    pub server_time: DateTime<Utc>,
    /// The browser can call and ring: the company's calling is on, with its
    /// credential connection set. False: show "calling is off".
    pub calling_enabled: bool,
    /// Which calls the company records: `off`, `inbound` or `both`.
    pub recording_mode: RecordingMode,
    /// The numbers the user may call from (direct lines first); empty: the
    /// user cannot call.
    pub caller_ids: Vec<CallerId>,
    /// The caller ID calls use unless another is picked: the user's default
    /// while it is theirs, else their first direct line, else the first.
    pub default_number_id: Option<i32>,
    /// The user's phone settings (the verified cell for Call via my cell).
    pub settings: PhoneSettings,
    /// Whether calls ring the user now (Do not disturb, working hours).
    pub availability: Availability,
}

/// Body of `POST /v1/phone/web/devices`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebDeviceRegistration {
    /// Shown in "Signed-in devices", e.g. "Chrome on Windows" (cut to 100
    /// characters; empty = "Browser").
    #[serde(default)]
    pub device_name: String,
}

/// This browser's phone.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebPhone {
    /// The browser's phone device: send it as the `X-Granite-Device-Id` SIP
    /// header of every call, and use it in `/v1/phone/web/devices/{id}/...`.
    pub id: i64,
    /// Its device session (`SignedInDevice.id` in `GET /v1/me/devices`).
    pub session_id: i64,
    /// Its SIP user on Telnyx (`gencred...`).
    pub sip_username: String,
    /// The key that signs this browser's calls (`X-Granite-Call-Auth`, made
    /// as the app makes it): base64url of 32 bytes. Keep it in memory only;
    /// registering again gives it again.
    pub call_key: String,
    /// Our clock now (RFC 3339), to sign calls with.
    #[serde(serialize_with = "js_date_required")]
    pub server_time: DateTime<Utc>,
}

/// Body of `PUT /v1/phone/web/devices/{id}/presence`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct WebPresence {
    /// True: the tab is connected to Telnyx and rings (for the next 150
    /// seconds; say it again every minute). False: it stops ringing at once.
    pub online: bool,
}

/// The company's Telnyx setup when a browser can call: else 403
/// `calling_disabled`, or 503 when Telnyx is not set up on this server.
async fn web_calling(state: &AppState, company_id: i32) -> Result<CompanyConfig, ApiError> {
    let config = company::load(&state.pool, company_id)
        .await?
        .filter(|config| config.calling_on() && config.credential_connection_id.is_some())
        .ok_or_else(|| denied("calling_disabled"))?;
    if !state.telnyx.is_enabled() {
        return Err(ApiError::Unavailable(
            "Calling is not set up on the server.".into(),
        ));
    }
    Ok(config)
}

/// The request's web session id (it verified already: the extractor read it).
fn web_session(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    request_session_id(headers, &state.session_secret).ok_or(ApiError::Unauthorized)
}

/// A browser phone of the request's web session.
#[derive(Debug, FromRow)]
struct WebDevice {
    id: i64,
    session_id: i64,
    /// It and its device session are signed in (see [`LIVE_DEVICE`]).
    live: bool,
}

/// Browser phone `id` of web session `web_session` of the user, in the
/// company the request acts in: 404 when there is none, 401
/// `device_revoked` once it was signed out (the dock turns itself off).
async fn web_device(
    pool: &MySqlPool,
    web_session: &str,
    user: &SessionUser,
    id: i64,
) -> ApiResult<WebDevice> {
    let device: Option<WebDevice> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT CAST(pd.id AS SIGNED) AS id, CAST(ds.id AS SIGNED) AS session_id,
                ({LIVE_DEVICE}) AS live
           FROM phone_devices pd
           JOIN device_sessions ds ON ds.id = pd.device_session_id
          WHERE pd.id = ? AND pd.platform = 'web' AND ds.web_session_id = ?
            AND ds.user_id = ? AND ds.company_id = ? AND pd.company_id = ds.company_id"
    )))
    .bind(id)
    .bind(web_session)
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    match device {
        Some(device) if device.live => Ok(device),
        Some(_) => Err(ApiError::Unauthenticated("device_revoked".into())),
        None => Err(ApiError::NotFound),
    }
}

/// Notes that the browser phone is in use (at most every 5 minutes), so its
/// sign-in does not idle out while the dock is open.
async fn seen(pool: &MySqlPool, session_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP()
          WHERE id = ? AND (last_seen_at IS NULL
                            OR last_seen_at < UTC_TIMESTAMP() - INTERVAL ? SECOND)",
    )
    .bind(session_id)
    .bind(SEEN_EVERY_SECS)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get what the phone dock needs
///
/// The CRM's phone dock reads this when it starts: whether the browser can
/// call (the company's calling is on), the recording mode (show the notice;
/// the Record button), the caller IDs and the default one, the user's
/// phone settings (a verified cell enables Call via my cell) and
/// availability, and our clock (calls are signed with it). Session cookie
/// (employees, admins, superusers). Next: `POST /v1/phone/web/devices`.
#[utoipa::path(
    get,
    path = "/v1/phone/web/bootstrap",
    operation_id = "get_web_phone_bootstrap",
    tag = "phone",
    responses(
        (status = 200, description = "The dock's start", body = WebPhoneBootstrap),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn web_bootstrap(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<WebPhoneBootstrap>> {
    let pool = &state.pool;
    let config = company::load(pool, user.company_id).await?;
    let calling_enabled = config
        .as_ref()
        .is_some_and(|config| config.calling_on() && config.credential_connection_id.is_some());
    let caller_ids = lines::caller_ids(pool, user.company_id, user.id).await?;
    let stored = settings::load(pool, user.id, user.company_id).await?;
    let now = Utc::now();
    Ok(Json(WebPhoneBootstrap {
        server_time: now,
        calling_enabled,
        recording_mode: config
            .as_ref()
            .map_or(RecordingMode::Off, |config| config.recording_mode),
        default_number_id: default_caller_id(&caller_ids, stored.default_caller_number_id)
            .map(|caller| caller.number_id),
        settings: stored.wire(&caller_ids),
        availability: stored.availability(now),
        caller_ids,
    }))
}

/// Finds the live browser phone of the web session for the user in their
/// company, or makes one (device session and phone device), in one
/// transaction that locks the web session, so two tabs make one phone.
/// Browser phones of the session in another company (a super admin who
/// switched) are returned to be signed out. (device id, device session id,
/// made now, others to sign out).
async fn find_or_make(
    pool: &MySqlPool,
    user: &SessionUser,
    web_session: &str,
    device_name: &str,
) -> ApiResult<(i64, i64, bool, Vec<i64>)> {
    let mut tx = pool.begin().await?;
    let locked: Option<String> =
        sqlx::query_scalar("SELECT id FROM sessions WHERE id = ? FOR UPDATE")
            .bind(web_session)
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        tx.rollback().await?;
        return Err(ApiError::Unauthorized);
    }
    let rows: Vec<(i64, i32, i32, Option<i64>)> = sqlx::query_as(
        "SELECT CAST(ds.id AS SIGNED), ds.user_id, ds.company_id, CAST(pd.id AS SIGNED)
           FROM device_sessions ds
           LEFT JOIN phone_devices pd ON pd.device_session_id = ds.id AND pd.revoked_at IS NULL
          WHERE ds.web_session_id = ? AND ds.revoked_at IS NULL
          ORDER BY ds.id",
    )
    .bind(web_session)
    .fetch_all(&mut *tx)
    .await?;
    let mut others = Vec::new();
    let mut found = None;
    for (session_id, user_id, company_id, device_id) in rows {
        match device_id {
            Some(device_id)
                if found.is_none() && user_id == user.id && company_id == user.company_id =>
            {
                found = Some((device_id, session_id));
            }
            _ => others.push(session_id),
        }
    }
    if let Some((device_id, session_id)) = found {
        sqlx::query(
            "UPDATE device_sessions SET device_name = ?, last_seen_at = UTC_TIMESTAMP() WHERE id = ?",
        )
        .bind(device_name)
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE phone_devices SET device_name = ? WHERE id = ?")
            .bind(device_name)
            .bind(device_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok((device_id, session_id, false, others));
    }
    // The token is never handed out: a browser phone signs in with the
    // cookie only. It ends with the web session (at most 2 months).
    let token = new_token().map_err(|err| ApiError::Internal(err.to_string()))?;
    let session_id = sqlx::query(
        "INSERT INTO device_sessions
           (user_id, company_id, token_hash, device_name, platform, app_version, expires_at,
            last_seen_at, web_session_id)
         SELECT ?, ?, ?, ?, 'web', NULL,
                LEAST(COALESCE(s.expiration_date, UTC_TIMESTAMP() + INTERVAL 2 MONTH),
                      COALESCE(s.created_date, UTC_TIMESTAMP()) + INTERVAL 2 MONTH),
                UTC_TIMESTAMP(), s.id
           FROM sessions s WHERE s.id = ?",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(token_hash(&token))
    .bind(device_name)
    .bind(web_session)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let session_id = i64::try_from(session_id)
        .map_err(|_| ApiError::Internal("device session id out of range".into()))?;
    let device_id = sqlx::query(
        "INSERT INTO phone_devices (device_session_id, user_id, company_id, platform, device_name)
         VALUES (?, ?, ?, 'web', ?)",
    )
    .bind(session_id)
    .bind(user.id)
    .bind(user.company_id)
    .bind(device_name)
    .execute(&mut *tx)
    .await?
    .last_insert_id();
    let device_id = i64::try_from(device_id)
        .map_err(|_| ApiError::Internal("phone device id out of range".into()))?;
    tx.commit().await?;
    Ok((device_id, session_id, true, others))
}

/// Register this browser as a phone
///
/// The CRM's phone dock registers the browser for calls with the session
/// cookie: the web session's browser phone (one per web session, so every
/// tab gets the same; idempotent: 201 the first time, 200 after) with its
/// Telnyx credential (working 30 days, extended with every login JWT), and
/// its call key, which signs the browser's calls (`X-Granite-Call-Auth`, as
/// the app does). It rings with the user's app phones while the dock says it
/// is connected (`PUT .../presence`). Signing out of the CRM signs it out.
/// 403 `calling_disabled` while the company's calling is off; 502 when
/// Telnyx did not make the credential (try again); 503 when Telnyx is not
/// set up on the server. Next: `POST /v1/phone/web/devices/{id}/token`.
#[utoipa::path(
    post,
    path = "/v1/phone/web/devices",
    operation_id = "register_web_phone",
    tag = "phone",
    request_body = WebDeviceRegistration,
    responses(
        (status = 201, description = "Registered", body = WebPhone),
        (status = 200, description = "This web session's browser phone, registered before", body = WebPhone),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "`calling_disabled`, or not an employee or admin", body = ErrorBody),
        (status = 502, description = "Telnyx did not make the credential; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn register_web_phone(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    headers: HeaderMap,
    JsonBody(input): JsonBody<WebDeviceRegistration>,
) -> ApiResult<Response> {
    let web_session = web_session(&state, &headers)?;
    let config = web_calling(&state, user.company_id).await?;
    let name = clip(&input.device_name, DEVICE_NAME_CHARS);
    let name = if name.is_empty() {
        "Browser".into()
    } else {
        name
    };
    let (device_id, session_id, created, others) =
        find_or_make(&state.pool, &user, &web_session, &name).await?;
    for other in others {
        revoke_device_session(&state, other, RevokeReason::ByUser).await?;
    }
    let (_, sip_username) = devices::ensure_credential(&state, Some(&config), device_id)
        .await
        .map_err(credential_failed)?
        .ok_or_else(|| denied("calling_disabled"))?;
    let body = WebPhone {
        id: device_id,
        session_id,
        sip_username,
        call_key: encoded_call_key(&state.session_secret, session_id),
        server_time: Utc::now(),
    };
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, [(CACHE_CONTROL, "no-store")], Json(body)).into_response())
}

/// Get a Telnyx login JWT for this browser
///
/// Mints a Telnyx login JWT (about 24 hours) for this browser phone's SIP
/// credential and extends the credential to 30 days, like the app's
/// `POST /v1/phone/devices/{id}/token`, with the same limits: one a minute
/// and six an hour per phone (429 with `retryAfterSecs`; a failed mint does
/// not count), so the dock keeps the JWT until it expires instead of
/// minting one per page. 404 when `id` is not this web session's browser
/// phone, 401 `device_revoked` once it was signed out, 403
/// `calling_disabled` while the company's calling is off.
#[utoipa::path(
    post,
    path = "/v1/phone/web/devices/{id}/token",
    operation_id = "mint_web_phone_token",
    tag = "phone",
    params(("id" = i64, Path, description = "This browser's phone (`WebPhone.id`)")),
    responses(
        (status = 200, description = "The JWT", body = DeviceToken),
        (status = 401, description = "No valid session, or `device_revoked`", body = ErrorBody),
        (status = 403, description = "`calling_disabled`, or not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not this web session's browser phone", body = ErrorBody),
        (status = 429, description = "`rate_limited`: one a minute, six an hour", body = RateLimited),
        (status = 502, description = "Telnyx failed; try again", body = ErrorBody),
        (status = 503, description = "Telnyx is not set up on the server", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn mint_web_phone_token(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    let web_session = web_session(&state, &headers)?;
    let pool = &state.pool;
    let device = web_device(pool, &web_session, &user, id).await?;
    let config = web_calling(&state, user.company_id).await?;
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
    seen(pool, device.session_id).await?;
    let expires_at = jwt_expiry(&jwt).unwrap_or_else(|| Utc::now() + Duration::hours(JWT_HOURS));
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(DeviceToken { jwt, expires_at }),
    )
        .into_response())
}

/// Say whether this browser rings
///
/// The dock says `online: true` once its Telnyx client is connected and
/// every minute after: incoming calls ring the browser for the next 150
/// seconds (with the user's app phones). `online: false` (the dock closed,
/// the phone turned off on this computer) stops it ringing at once. 404 when
/// `id` is not this web session's browser phone, 401 `device_revoked` once
/// it was signed out.
#[utoipa::path(
    put,
    path = "/v1/phone/web/devices/{id}/presence",
    operation_id = "set_web_phone_presence",
    tag = "phone",
    params(("id" = i64, Path, description = "This browser's phone (`WebPhone.id`)")),
    request_body = WebPresence,
    responses(
        (status = 204, description = "Saved"),
        (status = 401, description = "No valid session, or `device_revoked`", body = ErrorBody),
        (status = 403, description = "Not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not this web session's browser phone", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_web_phone_presence(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
    JsonBody(input): JsonBody<WebPresence>,
) -> ApiResult<StatusCode> {
    let web_session = web_session(&state, &headers)?;
    let pool = &state.pool;
    let device = web_device(pool, &web_session, &user, id).await?;
    sqlx::query(
        "UPDATE phone_devices
            SET online_until = IF(?, UTC_TIMESTAMP() + INTERVAL ? SECOND, NULL)
          WHERE id = ? AND revoked_at IS NULL",
    )
    .bind(input.online)
    .bind(ONLINE_SECS)
    .bind(device.id)
    .execute(pool)
    .await?;
    seen(pool, device.session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Decline the call ringing this browser
///
/// Best effort, after the user declined in the dock: the newest call still
/// ringing this browser has its leg marked declined, and when nobody
/// answered and no other person's phone rings, the caller goes on to the
/// number's no-answer action (AI Assistant, voicemail or forward), which
/// stops the user's other phones too (as the app's
/// `POST /v1/phone/calls/decline`). On a shared line that also rings
/// others, only this browser stops. Nothing ringing: nothing to do (204).
/// 404 when `id` is not this web session's browser phone.
#[utoipa::path(
    post,
    path = "/v1/phone/web/devices/{id}/decline",
    operation_id = "decline_web_phone_call",
    tag = "phone",
    params(("id" = i64, Path, description = "This browser's phone (`WebPhone.id`)")),
    responses(
        (status = 204, description = "Done (or nothing to do)"),
        (status = 401, description = "No valid session, or `device_revoked`", body = ErrorBody),
        (status = 403, description = "Not an employee or admin", body = ErrorBody),
        (status = 404, description = "Not this web session's browser phone", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn decline_web_phone_call(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let web_session = web_session(&state, &headers)?;
    let pool = &state.pool;
    let device = web_device(pool, &web_session, &user, id).await?;
    let leg: Option<(i64, i64, Option<String>, bool)> = sqlx::query_as(
        "SELECT CAST(l.id AS SIGNED), CAST(pc.id AS SIGNED), pc.telnyx_call_sid,
                (pc.answered_at IS NULL
                 AND NOT EXISTS (SELECT 1 FROM phone_call_legs a
                                  WHERE a.call_id = pc.id
                                    AND a.status IN ('answered', 'in-progress', 'in_progress'))
                 AND NOT EXISTS (SELECT 1 FROM phone_call_legs o
                                   JOIN phone_devices od ON od.id = o.device_id
                                  WHERE o.call_id = pc.id AND od.user_id <> ?
                                    AND o.status IN ('queued', 'initiated', 'ringing'))) AS stop
           FROM phone_call_legs l JOIN phone_calls pc ON pc.id = l.call_id
          WHERE l.device_id = ? AND pc.company_id = ? AND pc.status = 'ringing'
            AND pc.ended_at IS NULL AND l.status IN ('queued', 'initiated', 'ringing')
          ORDER BY pc.id DESC
          LIMIT 1",
    )
    .bind(user.id)
    .bind(device.id)
    .bind(user.company_id)
    .fetch_optional(pool)
    .await?;
    let Some((leg_id, call_id, parent_sid, stop)) = leg else {
        return Ok(StatusCode::NO_CONTENT);
    };
    sqlx::query("UPDATE phone_call_legs SET status = 'declined' WHERE id = ?")
        .bind(leg_id)
        .execute(pool)
        .await?;
    if !stop {
        return Ok(StatusCode::NO_CONTENT);
    }
    let config = company::load(pool, user.company_id).await?;
    let (Some(config), Some(parent_sid)) = (config, parent_sid) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let url = format!(
        "{}/dial-result?call={call_id}&declined=1",
        config.webhook_base(&state.phone)
    );
    if let Err(err) = state.telnyx.texml_redirect(&parent_sid, &url).await {
        tracing::warn!(call_id, error = %err, "declined call not redirected");
    }
    Ok(StatusCode::NO_CONTENT)
}
