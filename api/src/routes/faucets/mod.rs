//! Faucet models and their stock.
//!
//! - `GET /v1/faucets` → faucet models with free unit counts (`type`,
//!   `supplier_id`, `include_sold_out` query params)
//!
//! Replaces the loader of the Remix `employee.faucets.tsx` route
//! (`faucetQueryBuilder` in `app/utils/queries.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(handlers::list))
}
