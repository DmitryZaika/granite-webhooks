//! Sales: slabs (with their sinks and faucets) sold to a customer.
//!
//! - `GET  /v1/sales/{id}`        → the sale as the sell / edit form shows it
//! - `POST /v1/sales`             → sell slabs to a customer
//! - `PUT  /v1/sales/{id}`        → save the edited sale
//! - `POST /v1/sales/{id}/unsell` → cancel the sale and free its slabs
//!
//! Replaces `app/orm/contract.ts`, `app/utils/sellSlabAction.server.ts`,
//! `app/utils/contractsBackend.server.ts` and the Remix `api.unsell.$sale`
//! route.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::get, handlers::update))
        .routes(routes!(handlers::create))
        .routes(routes!(handlers::unsell))
}
