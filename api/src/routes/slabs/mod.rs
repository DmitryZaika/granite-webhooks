//! Slabs (`slab_inventory`): single slabs and remnants of a stone.
//!
//! - `PATCH /v1/slabs/{id}` → change a slab's length and width
//!
//! Listing slabs lives under the stone: `GET /v1/stones/{id}/slabs`.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::update))
}
