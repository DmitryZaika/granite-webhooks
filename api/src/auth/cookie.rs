//! Reads and writes the React Router `createCookieSessionStorage` cookie.
//!
//! Wire format (react-router `lib/server-runtime/cookies.ts` + `crypto.ts`):
//!
//! ```text
//! __session = percent_encode( base64(utf8(json)) + "." + base64_nopad(hmac_sha256(secret, base64(utf8(json)))) )
//! ```
//!
//! Keeping this byte-compatible lets the Lambda accept sessions Remix issued,
//! and later issue sessions itself without logging anyone out.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub const SESSION_COOKIE_NAME: &str = "__session";

type HmacSha256 = Hmac<Sha256>;

/// The fields of the Remix `SessionData` the API needs. Other keys (QBO tokens,
/// flash messages) are ignored on read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Super admins switch companies by storing the target here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_company_id: Option<i32>,
}

/// Value of the named cookie in a `Cookie` header, percent-decoded the way the
/// `cookie` npm package does it (first occurrence wins, quotes stripped).
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    cookie_values(header, name).next()
}

/// Every value of the named cookie in a `Cookie` header, in order. A browser
/// sends two `__session` cookies when a host-only one (dev's API) sits next to
/// the shared-domain one (production's); only one verifies with our secret.
pub fn cookie_values<'a>(header: &'a str, name: &'a str) -> impl Iterator<Item = String> + 'a {
    header.split(';').filter_map(move |pair| {
        let (key, value) = pair.split_once('=')?;
        if key.trim() != name {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        Some(percent_decode_str(value).decode_utf8().ok()?.into_owned())
    })
}

/// Verifies the signature and decodes the session payload. `None` for any
/// tampered, truncated, or foreign cookie.
pub fn decode_session(value: &str, secret: &str) -> Option<SessionData> {
    let (payload, signature) = value.rsplit_once('.')?;
    let signature = STANDARD_NO_PAD
        .decode(signature.trim_end_matches('='))
        .ok()?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).ok()?;
    let json = STANDARD.decode(payload).ok()?;
    serde_json::from_slice(&json).ok()
}

/// Every key of a verified session, unknown ones (OAuth state, QuickBooks
/// tokens, flash messages) included, so a rewrite can keep them.
pub fn decode_session_map(
    value: &str,
    secret: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    decode_session(value, secret)?;
    let payload = value.rsplit_once('.')?.0;
    let json = STANDARD.decode(payload).ok()?;
    serde_json::from_slice(&json).ok()
}

/// Inverse of [`decode_session`]; returns the percent-encoded cookie value.
pub fn encode_session(data: &SessionData, secret: &str) -> String {
    encode_json(
        &serde_json::to_vec(data).expect("session data serializes"),
        secret,
    )
}

/// [`encode_session`] for a whole session map (see [`decode_session_map`]).
pub fn encode_session_map(
    data: &serde_json::Map<String, serde_json::Value>,
    secret: &str,
) -> String {
    encode_json(
        &serde_json::to_vec(data).expect("session data serializes"),
        secret,
    )
}

fn encode_json(json: &[u8], secret: &str) -> String {
    let payload = STANDARD.encode(json);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    let signature = STANDARD_NO_PAD.encode(mac.finalize().into_bytes());
    utf8_percent_encode(&format!("{payload}.{signature}"), COOKIE_VALUE_ENCODE).to_string()
}

/// Decodes an unsigned React Router cookie (`createCookie` without secrets):
/// base64 of the UTF-8 JSON. `None` when it is not one.
pub fn decode_plain(value: &str) -> Option<serde_json::Value> {
    let json = STANDARD.decode(value.trim()).ok()?;
    serde_json::from_slice(&json).ok()
}

/// Inverse of [`decode_plain`]; returns the percent-encoded cookie value.
pub fn encode_plain(value: &serde_json::Value) -> String {
    let payload = STANDARD.encode(serde_json::to_vec(value).expect("JSON serializes"));
    utf8_percent_encode(&payload, COOKIE_VALUE_ENCODE).to_string()
}

/// Where the session cookie is set: the shared parent domain (so the API
/// and every company subdomain see it), and whether it is HTTPS only.
#[derive(Debug, Clone, Default)]
pub struct CookieConfig {
    /// `Domain` attribute, e.g. `.granite-manager.com`; host-only when None.
    pub domain: Option<String>,
    /// `Secure` attribute (off only for plain-HTTP local development).
    pub secure: bool,
}

/// Seconds a session lives (`SESSION_MAX_AGE_SECONDS`: 60 days).
pub const SESSION_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 60;

impl CookieConfig {
    fn attributes(&self, max_age: i64, with_domain: bool) -> String {
        let mut attributes = format!("Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax");
        if self.secure {
            attributes.push_str("; Secure");
        }
        if with_domain && let Some(domain) = &self.domain {
            attributes.push_str("; Domain=");
            attributes.push_str(domain);
        }
        attributes
    }

    /// A `Set-Cookie` value for another `HttpOnly` cookie of the app, on the
    /// session's domain.
    pub fn set_cookie(&self, name: &str, value: &str, max_age: i64) -> String {
        format!("{name}={value}; {}", self.attributes(max_age, true))
    }

    /// `Set-Cookie` values that store `value` as the session. With a shared
    /// domain, a leftover host-only session cookie is cleared first so the
    /// browser never sends two.
    pub fn set_session(&self, value: &str) -> Vec<String> {
        let mut cookies = Vec::new();
        if self.domain.is_some() {
            cookies.push(format!(
                "{SESSION_COOKIE_NAME}=; {}",
                self.attributes(0, false)
            ));
        }
        cookies.push(format!(
            "{SESSION_COOKIE_NAME}={value}; {}",
            self.attributes(SESSION_MAX_AGE_SECONDS, true)
        ));
        cookies
    }

    /// `Set-Cookie` values that delete the session cookie (shared and host-only).
    pub fn clear_session(&self) -> Vec<String> {
        let mut cookies = vec![format!(
            "{SESSION_COOKIE_NAME}=; {}",
            self.attributes(0, true)
        )];
        if self.domain.is_some() {
            cookies.push(format!(
                "{SESSION_COOKIE_NAME}=; {}",
                self.attributes(0, false)
            ));
        }
        cookies
    }
}

/// `encodeURIComponent` leaves these unescaped.
const COOKIE_VALUE_ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

#[cfg(test)]
mod tests {
    use super::*;

    // Generated by react-router 7 `createCookieSessionStorage` with
    // secrets: ['test-secret'] (see api/README.md "Auth").
    const WITH_FLASH: &str = "__session=eyJzZXNzaW9uSWQiOiIxMTExMTExMS0yMjIyLTQzMzMtODQ0NC01NTU1NTU1NTU1NTUiLCJhY3RpdmVDb21wYW55SWQiOjEwMSwiX19mbGFzaF9tZXNzYWdlX18iOnsidGl0bGUiOiLQn9GA0LjQstC10YIg4pyTIiwidmFyaWFudCI6InN1Y2Nlc3MifX0%3D.lS%2B1F9jpvMgIZtkAUO5F7ioiULaxhq4xxP0HP59ekjA";
    const PLAIN: &str = "eyJzZXNzaW9uSWQiOiJhYWFhYWFhYS1iYmJiLTRjY2MtOGRkZC1lZWVlZWVlZWVlZWUifQ%3D%3D.MvwdWs%2B1qdYUAOeCazF9MUALVn3bGB4xsJ6ijvPVX6A";

    #[test]
    fn decodes_react_router_cookie_with_unicode_flash() {
        let header = format!("theme=dark; {WITH_FLASH}; other=1");
        let value = cookie_value(&header, SESSION_COOKIE_NAME).unwrap();
        let data = decode_session(&value, "test-secret").unwrap();
        assert_eq!(
            data.session_id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(data.active_company_id, Some(101));
    }

    #[test]
    fn finds_our_session_among_same_named_cookies() {
        let header = format!("__session=foreign.c2lnbmF0dXJl; theme=dark; __session={PLAIN}");
        let ours = cookie_values(&header, SESSION_COOKIE_NAME)
            .find_map(|value| decode_session(&value, "test-secret"))
            .unwrap();
        assert_eq!(
            ours.session_id.as_deref(),
            Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee")
        );
        assert_eq!(cookie_values(&header, SESSION_COOKIE_NAME).count(), 2);
    }

    #[test]
    fn encode_matches_react_router_byte_for_byte() {
        let data = SessionData {
            session_id: Some("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into()),
            active_company_id: None,
        };
        assert_eq!(encode_session(&data, "test-secret"), PLAIN);
    }

    #[test]
    fn rejects_wrong_secret_and_tampering() {
        let value = cookie_value(&format!("__session={PLAIN}"), SESSION_COOKIE_NAME).unwrap();
        assert!(decode_session(&value, "other-secret").is_none());
        let tampered = value.replacen("eyJ", "eyK", 1);
        assert!(decode_session(&tampered, "test-secret").is_none());
        assert!(decode_session("no-dot", "test-secret").is_none());
    }

    #[test]
    fn session_maps_keep_unknown_keys() {
        let header = format!("theme=dark; {WITH_FLASH}");
        let value = cookie_value(&header, SESSION_COOKIE_NAME).unwrap();
        let mut map = decode_session_map(&value, "test-secret").unwrap();
        assert!(map.contains_key("__flash_message__"));
        map.insert("activeCompanyId".into(), 100.into());
        let encoded = encode_session_map(&map, "test-secret");
        let decoded = cookie_value(&format!("__session={encoded}"), SESSION_COOKIE_NAME).unwrap();
        let back = decode_session_map(&decoded, "test-secret").unwrap();
        assert_eq!(back["activeCompanyId"], 100);
        assert!(back.contains_key("__flash_message__"));
        assert!(decode_session_map(&decoded, "other").is_none());
    }

    #[test]
    fn cookie_attributes() {
        let shared = CookieConfig {
            domain: Some(".granite-manager.com".into()),
            secure: true,
        };
        let set = shared.set_session("abc");
        assert_eq!(
            set[0],
            "__session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            set[1],
            "__session=abc; Path=/; Max-Age=5184000; HttpOnly; SameSite=Lax; Secure; Domain=.granite-manager.com"
        );
        assert_eq!(shared.clear_session().len(), 2);
        let local = CookieConfig::default();
        assert_eq!(
            local.set_session("abc"),
            ["__session=abc; Path=/; Max-Age=5184000; HttpOnly; SameSite=Lax"]
        );
    }

    #[test]
    fn missing_cookie_is_none() {
        assert!(cookie_value("a=1; b=2", SESSION_COOKIE_NAME).is_none());
    }
}
