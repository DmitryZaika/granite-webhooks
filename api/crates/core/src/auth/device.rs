//! The Granite Phone app's sign-in: a device token sent as `X-Granite-Device`
//! (CloudFront drops `Authorization` on GET requests).
//!
//! - [`DeviceUser`]: the device token only (the app's own routes).
//! - [`PhoneCaller`]: the device token when the header is there, else the
//!   web session cookie with [`super::EmployeeUser`]'s rules (routes the
//!   app and the CRM share).
//!
//! Tokens are 32 random bytes, base64url without padding (43 characters),
//! stored only as their SHA-256 (hex). A device session signs in while it is
//! not revoked (however old or long unused), its
//! company is not deleted, and its user is live and still an employee,
//! admin or superuser of the session's company. All of it is checked on
//! every request, so a revoked phone, a deleted company or a deleted,
//! demoted or moved user is out at once (401).
//!
//! Each device session also has a call key ([`call_key`]): the phone signs
//! the calls it places with it (`X-Granite-Call-Auth`). It is derived from
//! `SESSION_SECRET` and the session id, given to the app once at sign-in
//! and never stored.

use super::session::{SessionUser, effective_user, find_user, user_positions};
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use sqlx::FromRow;

/// The header the app sends its device token in.
pub const DEVICE_HEADER: &str = "x-granite-device";
/// Length of a device token (32 bytes, base64url without padding).
pub const TOKEN_LEN: usize = 43;
/// The `expires_at` a device session is given: shown, never enforced. A device
/// session ends only when it is signed out or revoked, never by time.
pub const SESSION_MONTHS: u32 = 12;
/// `last_seen_at` is written at most this often.
const SEEN_EVERY_SECONDS: i64 = 300;

/// A new device token. Give it to the app once; store only [`token_hash`].
pub fn new_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// How a token is stored and looked up: its SHA-256, hex.
pub fn token_hash(token: &str) -> String {
    super::sha256_hex(token)
}

/// What a call key is derived from, before the device session id.
const CALL_KEY_CONTEXT: &str = "granite-phone-call-key:";

/// The key device session `session_id`'s phone signs its calls with
/// (`X-Granite-Call-Auth`): HMAC-SHA256, keyed with the bytes of
/// `SESSION_SECRET` (`secret`), of `granite-phone-call-key:{session id}`.
///
/// Nothing is stored: the app gets it once at sign-in ([`encoded_call_key`])
/// and the server derives it again for each call, so a copy of the database
/// cannot sign calls. A new `SESSION_SECRET` changes every key (phones then
/// sign in again before they can call).
pub fn call_key(secret: &str, session_id: i64) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes keys of any length");
    mac.update(format!("{CALL_KEY_CONTEXT}{session_id}").as_bytes());
    let mut key = [0; 32];
    key.copy_from_slice(&mac.finalize().into_bytes());
    key
}

/// [`call_key`] as the app gets it at sign-in (`callKey`): base64url without
/// padding, 43 characters.
pub fn encoded_call_key(secret: &str, session_id: i64) -> String {
    URL_SAFE_NO_PAD.encode(call_key(secret, session_id))
}

/// The shape of a token we issue; anything else is refused unread.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The device token of a request, when it has the header.
pub(super) fn device_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(DEVICE_HEADER)
        .map(|value| value.to_str().unwrap_or_default().trim().to_string())
}

/// A request signed in with a device token.
#[derive(Debug, Clone)]
pub struct DeviceUser {
    /// The user, as a session would load them (the company is the device
    /// session's, the user's own).
    pub user: SessionUser,
    /// The device session (`device_sessions.id`).
    pub session_id: i64,
    /// This session's live phone device (`phone_devices.id`), once the app
    /// registered it (`POST /v1/phone/devices`).
    pub device_id: Option<i64>,
}

#[derive(FromRow)]
struct LiveSession {
    session_id: i64,
    user_id: i32,
    company_id: i32,
    device_id: Option<i64>,
    /// `last_seen_at` is due for a write.
    stale: bool,
}

/// The device session of `token`, when it still signs in.
pub(super) async fn authenticate_device(
    token: &str,
    state: &AppState,
) -> Result<DeviceUser, ApiError> {
    if !well_formed(token) {
        return Err(ApiError::Unauthorized);
    }
    let session: LiveSession = sqlx::query_as(
        "SELECT CAST(ds.id AS SIGNED) AS session_id, ds.user_id, ds.company_id,
                CAST(pd.id AS SIGNED) AS device_id,
                (ds.last_seen_at IS NULL
                 OR ds.last_seen_at < UTC_TIMESTAMP() - INTERVAL ? SECOND) AS stale
           FROM device_sessions ds
           JOIN company co ON co.id = ds.company_id AND co.deleted_at IS NULL
           LEFT JOIN phone_devices pd ON pd.device_session_id = ds.id AND pd.revoked_at IS NULL
          WHERE ds.token_hash = ?
            AND ds.revoked_at IS NULL",
    )
    .bind(SEEN_EVERY_SECONDS)
    .bind(token_hash(token))
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::Unauthorized)?;
    let user = find_user(&state.pool, session.user_id, session.company_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let positions = user_positions(&state.pool, user.id).await?;
    let user = effective_user(user, &positions, None);
    if !(user.is_employee || user.is_admin || user.is_superuser) {
        return Err(ApiError::Unauthorized);
    }
    if session.stale {
        sqlx::query("UPDATE device_sessions SET last_seen_at = UTC_TIMESTAMP() WHERE id = ?")
            .bind(session.session_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(DeviceUser {
        user,
        session_id: session.session_id,
        device_id: session.device_id,
    })
}

impl FromRequestParts<AppState> for DeviceUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = device_token(parts).ok_or(ApiError::Unauthorized)?;
        let device = authenticate_device(&token, state).await?;
        super::check_sections(parts, state, &device.user).await?;
        Ok(device)
    }
}

/// How a [`PhoneCaller`] signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerAuth {
    /// The app's device token.
    Device {
        /// The device session.
        session_id: i64,
        /// Its live phone device, once registered.
        device_id: Option<i64>,
    },
    /// The CRM's session cookie.
    Cookie,
}

/// A request from the phone app (device token) or the CRM (session cookie).
///
/// The cookie is taken from employees, admins and superusers only. When the
/// `X-Granite-Device` header is there it decides alone: a bad token is 401
/// even with a cookie.
#[derive(Debug, Clone)]
pub struct PhoneCaller {
    pub user: SessionUser,
    pub auth: CallerAuth,
}

impl PhoneCaller {
    /// The caller's device session, when it came with a device token.
    pub const fn session_id(&self) -> Option<i64> {
        match self.auth {
            CallerAuth::Device { session_id, .. } => Some(session_id),
            CallerAuth::Cookie => None,
        }
    }

    /// The caller's phone device, when it came with a device token and the
    /// app registered it.
    pub const fn device_id(&self) -> Option<i64> {
        match self.auth {
            CallerAuth::Device { device_id, .. } => device_id,
            CallerAuth::Cookie => None,
        }
    }
}

impl FromRequestParts<AppState> for PhoneCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(token) = device_token(parts) {
            let device = authenticate_device(&token, state).await?;
            super::check_sections(parts, state, &device.user).await?;
            return Ok(Self {
                user: device.user,
                auth: CallerAuth::Device {
                    session_id: device.session_id,
                    device_id: device.device_id,
                },
            });
        }
        let user = super::authenticate(parts, state).await?;
        if user.is_employee || user.is_admin || user.is_superuser {
            Ok(Self {
                user,
                auth: CallerAuth::Cookie,
            })
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_43_random_base64url_characters() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), TOKEN_LEN);
        assert!(well_formed(&a) && well_formed(&b));
        assert_ne!(a, b);
        assert_eq!(token_hash(&a).len(), 64);
        assert_ne!(token_hash(&a), token_hash(&b));
    }

    #[test]
    fn call_keys_are_derived_from_the_secret_and_the_session() {
        // Known answers (Node: createHmac('sha256', secret)
        // .update('granite-phone-call-key:' + id).digest('base64url')).
        assert_eq!(
            encoded_call_key("test-secret", 950_041),
            "XFqc_6MqQzpyK4N67uAzWyx_YKx1DC09NWC6PsXbC4g"
        );
        assert_eq!(
            encoded_call_key("test-secret", 950_043),
            "iamWLYOCMt2SqHmKZ3keLLcs98C1cV4cYl2cRTAUJfs"
        );
        assert_eq!(
            encoded_call_key("another-secret", 950_041),
            "Q8Q167FAOci3M8tR6z0ryg-ozxGgD1oE-ZZzD54Jv2w"
        );
        assert_eq!(encoded_call_key("test-secret", 1).len(), 43);
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(encoded_call_key("test-secret", 950_041))
                .unwrap(),
            call_key("test-secret", 950_041)
        );
    }

    #[test]
    fn other_shapes_are_not_tokens() {
        assert!(!well_formed(""));
        assert!(!well_formed(&"a".repeat(42)));
        assert!(!well_formed(&"a".repeat(44)));
        assert!(!well_formed(&format!("{}=", "a".repeat(42))));
        assert!(!well_formed(&format!("{}+", "a".repeat(42))));
        assert!(well_formed(&"a-_9".repeat(11)[..43]));
    }
}
