//! AI helpers for writing messages.
//!
//! - `POST /v1/ai/improve` → rewrite a draft email or text
//!
//! Replaces the Remix `api.aiImprove.email` route.

pub mod improve;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(improve::improve))
}
