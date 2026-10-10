//! Stones (materials the company sells) and their slab inventory.
//!
//! - `GET /v1/stones` → displayed stones with slab counts (`type`, `supplier_id`,
//!   `color_id`, `level`, `finishing`, `include_sold_out` query params)
//!
//! Replaces the loader of the Remix `employee.stones.tsx` route
//! (`stoneQueryBuilder` in `app/utils/queries.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::list))
}
