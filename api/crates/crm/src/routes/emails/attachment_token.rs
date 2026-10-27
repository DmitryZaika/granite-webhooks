//! Proof that a user uploaded an email attachment URL.
//!
//! Sending reads attachments by a URL from the request body. Without proof
//! of origin any employee could name another company's file and mail it to
//! themselves. `POST /v1/uploads` (folder `emails`) issues this token; the
//! send route refuses URLs with neither a valid token nor a link to a thread
//! the caller may read. Same HMAC as the Remix `signAttachmentUrl`, so
//! scheduled sends the Remix cron delivers keep working.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::fmt::Write;

type HmacSha256 = Hmac<Sha256>;

fn mac(secret: &str, url: &str, user_id: i32) -> HmacSha256 {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{user_id}:{url}").as_bytes());
    mac
}

/// Hex HMAC-SHA256 of `"{user_id}:{url}"`.
pub fn sign(secret: &str, url: &str, user_id: i32) -> String {
    mac(secret, url, user_id)
        .finalize()
        .into_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

pub fn verify(secret: &str, url: &str, user_id: i32, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return false;
    };
    let Some(bytes) = decode_hex(token) else {
        return false;
    };
    mac(secret, url, user_id).verify_slice(&bytes).is_ok()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(value.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_like_the_remix_app() {
        // node -e "require('crypto').createHmac('sha256','s').update('7:u').digest('hex')"
        assert_eq!(
            sign("s", "u", 7),
            "40499754caecf3ad98ebed2c5a256e52405b9725a578c8588e63836bdc3edce7"
        );
        assert!(verify("s", "u", 7, Some(&sign("s", "u", 7))));
        assert!(!verify("s", "u", 8, Some(&sign("s", "u", 7))));
        assert!(!verify("s", "v", 7, Some(&sign("s", "u", 7))));
        assert!(!verify("s", "u", 7, None));
        assert!(!verify("s", "u", 7, Some("zz")));
    }
}
