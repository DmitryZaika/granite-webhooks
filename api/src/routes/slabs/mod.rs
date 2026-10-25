//! Slabs (`slab_inventory`): single slabs and remnants of a stone.
//!
//! - `GET /v1/slabs?id=..` → slabs by id with their stone
//! - `PATCH /v1/slabs/{id}` → change a slab's size and/or bundle label
//! - `DELETE /v1/slabs/{id}` → remove a slab from the yard
//!
//! Listing slabs lives under the stone: `GET /v1/stones/{id}/slabs`.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list))
        .routes(routes!(handlers::update, handlers::delete))
}
