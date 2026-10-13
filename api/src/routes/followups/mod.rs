//! AI follow-ups: the company rule the follow-up writer obeys.
//!
//! - `GET /v1/followups/instruction` → the company's AI follow-up instruction
//! - `PUT /v1/followups/instruction` → save it (empty deletes it)
//! - `GET /v1/followups/timeline` → the conversation the writer reads
//!
//! Replaces the Remix `api.followups.ai-instruction` and
//! `api.followups.timeline` routes.

pub mod instruction;
pub mod timeline;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(instruction::get, instruction::put))
        .routes(routes!(timeline::timeline))
}
