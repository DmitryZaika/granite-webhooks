//! The `state` of an OAuth connect (`QuickBooks`, `RingCentral`).
//!
//! The provider sends it back unchanged with the code; it proves the code
//! answers a connect this user started for this company within the last 15
//! minutes (Remix kept it in the session cookie; the API signs it instead):
//! `<provider>.<company>.<user>.<expires>.<nonce>.<hmac>`, the HMAC keyed
//! with `SESSION_SECRET`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

const LIFETIME_SECS: i64 = 15 * 60;

fn mac(secret: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// A state for `provider` (`quickbooks`, `ringcentral`).
pub fn sign(secret: &str, provider: &str, company_id: i32, user_id: i32) -> String {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).expect("system randomness");
    let expires = chrono::Utc::now().timestamp() + LIFETIME_SECS;
    let payload = format!(
        "{provider}.{company_id}.{user_id}.{expires}.{}",
        URL_SAFE_NO_PAD.encode(nonce)
    );
    let signature = mac(secret, &payload);
    format!("{payload}.{signature}")
}

/// Whether `state` is one this server signed for the provider, company and
/// user, and has not expired.
pub fn verify(secret: &str, state: &str, provider: &str, company_id: i32, user_id: i32) -> bool {
    let Some((payload, signature)) = state.rsplit_once('.') else {
        return false;
    };
    let expected = mac(secret, payload);
    if expected.len() != signature.len()
        || expected
            .bytes()
            .zip(signature.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            != 0
    {
        return false;
    }
    let parts: Vec<&str> = payload.split('.').collect();
    let [p, company, user, expires, _nonce] = parts.as_slice() else {
        return false;
    };
    *p == provider
        && company.parse::<i32>().ok() == Some(company_id)
        && user.parse::<i32>().ok() == Some(user_id)
        && expires
            .parse::<i64>()
            .is_ok_and(|expires| expires > chrono::Utc::now().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_is_bound_to_its_provider_company_and_user() {
        let state = sign("s", "quickbooks", 100, 7);
        assert!(verify("s", &state, "quickbooks", 100, 7));
        assert!(!verify("s", &state, "ringcentral", 100, 7));
        assert!(!verify("s", &state, "quickbooks", 101, 7));
        assert!(!verify("s", &state, "quickbooks", 100, 8));
        assert!(!verify("other", &state, "quickbooks", 100, 7));
        assert!(!verify("s", &format!("{state}x"), "quickbooks", 100, 7));
        assert!(!verify("s", "", "quickbooks", 100, 7));
    }

    #[test]
    fn an_expired_state_is_refused() {
        let payload = format!("quickbooks.100.7.{}.n", chrono::Utc::now().timestamp() - 1);
        let state = format!("{payload}.{}", mac("s", &payload));
        assert!(!verify("s", &state, "quickbooks", 100, 7));
    }
}
