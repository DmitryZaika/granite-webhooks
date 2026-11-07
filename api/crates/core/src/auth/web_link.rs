//! Web links: how the Granite Phone app opens a CRM page (a quote to edit,
//! say) already signed in, in the phone's browser.
//!
//! The app asks for a link with its device token (`POST /v1/auth/web-link`)
//! and opens it; the browser follows it (`GET /v1/auth/web-link/{token}`),
//! gets a session cookie for the same user and lands on the page. The token
//! is signed under `SESSION_SECRET`, names the device session, the user and
//! the page, and works for [`TOKEN_TTL_SECS`] only, while that device session
//! is still signed in.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Signed ahead of the claims, keeping these signatures apart from cookies',
/// agent tokens' and call keys'.
const SIGNED_PREFIX: &str = "web-link-v1.";
/// How long a link works after it is issued: enough to open it, too short to
/// be worth stealing.
pub const TOKEN_TTL_SECS: i64 = 120;
/// Longest page path a link may name.
pub const MAX_PATH_CHARS: usize = 512;

/// Whom a link signs in, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// The device session that asked for it (`device_sessions.id`).
    pub s: i64,
    /// Its user.
    pub u: i32,
    /// The CRM page: a path on the company's CRM host (`/employee/quote/12`).
    pub p: String,
    /// Unix seconds after which the link is refused.
    pub exp: i64,
}

fn signature(secret: &str, claims: &str) -> Option<HmacSha256> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(SIGNED_PREFIX.as_bytes());
    mac.update(claims.as_bytes());
    Some(mac)
}

/// A token for `claims`, signed under `secret` (URL-safe, no `/`).
pub fn issue(secret: &str, claims: &Claims) -> String {
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let mac = signature(secret, &body).expect("HMAC accepts any key length");
    let signed = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{body}.{signed}")
}

/// The claims of a token signed under `secret` that has not expired at `now`
/// (unix seconds) and names a page [`page_path`] accepts; `None` otherwise.
pub fn verify(secret: &str, token: &str, now: i64) -> Option<Claims> {
    let (body, signed) = token.split_once('.')?;
    let signed = URL_SAFE_NO_PAD.decode(signed).ok()?;
    signature(secret, body)?.verify_slice(&signed).ok()?;
    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
    (claims.exp > now && page_path(&claims.p).is_some()).then_some(claims)
}

/// `path` when it is a page of the CRM's own host.
///
/// It starts with one `/` (never `//` or `/\`, which a browser reads as
/// another host), has no backslash, whitespace or control character, and is
/// not too long.
pub fn page_path(path: &str) -> Option<&str> {
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && path.chars().count() <= MAX_PATH_CHARS
        && !path
            .chars()
            .any(|c| c == '\\' || c.is_whitespace() || c.is_control());
    ok.then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "test-secret";

    fn claims() -> Claims {
        Claims {
            s: 7,
            u: 100,
            p: "/employee/quote/12".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn a_link_opens_until_it_expires() {
        let token = issue(SECRET, &claims());
        assert!(!token.contains('/'));
        assert_eq!(verify(SECRET, &token, 999), Some(claims()));
        assert_eq!(verify(SECRET, &token, 1_000), None);
        assert_eq!(verify("another-secret", &token, 999), None);
    }

    #[test]
    fn a_changed_link_is_refused() {
        let token = issue(SECRET, &claims());
        let (_, signed) = token.split_once('.').unwrap();
        let other = Claims { u: 101, ..claims() };
        let forged = format!(
            "{}.{signed}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&other).unwrap())
        );
        assert_eq!(verify(SECRET, &forged, 999), None);
        assert_eq!(verify(SECRET, "nonsense", 999), None);
        assert_eq!(verify(SECRET, "", 999), None);
    }

    #[test]
    fn links_only_go_to_crm_pages() {
        for path in [
            "/employee/quote/12",
            "/employee/quotes/add",
            "/employee/quote/12?tab=lines#x",
        ] {
            assert_eq!(page_path(path), Some(path), "{path}");
        }
        for path in [
            "",
            "employee/quote/12",
            "//evil.example/x",
            "/\\evil.example",
            "https://evil.example/",
            "/a b",
            "/a\nb",
        ] {
            assert_eq!(page_path(path), None, "{path:?}");
        }
        assert_eq!(page_path(&format!("/{}", "a".repeat(MAX_PATH_CHARS))), None);
        // A signed link to another host is refused all the same.
        let token = issue(
            SECRET,
            &Claims {
                p: "//evil.example".into(),
                ..claims()
            },
        );
        assert_eq!(verify(SECRET, &token, 999), None);
    }
}
