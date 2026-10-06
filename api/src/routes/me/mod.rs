//! The signed-in user.
//!
//! - `GET /v1/me`           → effective session user (no extra query beyond auth)
//! - `GET /v1/me/positions` → position ids in the company the request acts in

mod handlers;
pub mod queries;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::me))
        .routes(routes!(handlers::positions))
}
