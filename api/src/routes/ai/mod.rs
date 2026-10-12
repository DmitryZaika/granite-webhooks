//! AI helpers for writing messages.
//!
//! - `POST /v1/ai/improve` → rewrite a draft email or text
//! - `POST /v1/ai/call-note` → a CRM note from a call transcript
//!
//! Replaces the Remix `api.aiImprove.email` and `api.aiSummarize.callNote` routes.

pub mod call_note;
pub mod improve;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(improve::improve))
        .routes(routes!(call_note::call_note))
}
