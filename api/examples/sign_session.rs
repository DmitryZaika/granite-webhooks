//! Prints a `Cookie` header for a session id, signed like the Remix app does.
//!
//! cargo run -q -p api --example sign_session -- <session_id> [active_company_id]
//!
//! Reads SESSION_SECRET from the environment (default: the local dev secret).
use api::auth::cookie::{SessionData, encode_session};

fn main() {
    let mut args = std::env::args().skip(1);
    let session_id = args
        .next()
        .expect("usage: sign_session <session_id> [active_company_id]");
    let active_company_id = args
        .next()
        .map(|id| id.parse().expect("company id is a number"));
    let secret =
        std::env::var("SESSION_SECRET").unwrap_or_else(|_| "local-dev-session-secret".into());
    let data = SessionData {
        session_id: Some(session_id),
        active_company_id,
    };
    println!("__session={}", encode_session(&data, &secret));
}
