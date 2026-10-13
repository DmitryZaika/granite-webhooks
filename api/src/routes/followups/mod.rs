//! AI follow-ups: the company rule the follow-up writer obeys.
//!
//! - `GET /v1/followups/instruction` → the company's AI follow-up instruction
//! - `PUT /v1/followups/instruction` → save it (empty deletes it)
//!
//! Replaces the Remix `api.followups.ai-instruction` route.

pub mod instruction;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(instruction::get, instruction::put))
}
