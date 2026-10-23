//! The signup funnel in `PostHog`, sent by the server: the payments app runs
//! no analytics script (PCI SAQ A). Events carry no PII. The distinct id is
//! derived from the signup id (the id itself is a bearer secret) and is
//! aliased to the new user's id at completion, where the main app's
//! `PostHog` identifies the user and continues the funnel.

use crate::state::AppState;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// `signup-<first 24 hex of SHA-256(signup id)>`.
pub fn distinct_id(signup_id: &str) -> String {
    let digest = Sha256::digest(signup_id.as_bytes());
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("signup-{hex}")
}

/// Captures a funnel event (`signup_started`, `signup_verified`, ...).
pub async fn capture(state: &AppState, event: &str, signup_id: &str, properties: Value) {
    crate::posthog::capture(&state.telephony_http, event, &distinct_id(signup_id), properties)
        .await;
}

/// Joins the signup's events to the user's.
pub async fn alias(state: &AppState, signup_id: &str, user_id: i32) {
    crate::posthog::capture(
        &state.telephony_http,
        "$create_alias",
        &user_id.to_string(),
        json!({ "alias": distinct_id(signup_id) }),
    )
    .await;
}
