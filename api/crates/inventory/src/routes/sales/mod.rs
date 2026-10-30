//! Sales: slabs (with their sinks and faucets) sold to a customer.
//!
//! - `GET  /v1/sales/{id}`        → the sale as the sell / edit form shows it
//! - `POST /v1/sales`             → sell slabs to a customer
//! - `PUT  /v1/sales/{id}`        → save the edited sale
//! - `POST /v1/sales/{id}/unsell` → cancel the sale and free its slabs
//! - `GET  /v1/sales/{id}/contract` → the filled-in contract PDF
//! - `GET  /v1/sales/daily`        → the sales of a day (the Daily sales widget)
//!
//! Replaces `app/orm/contract.ts`, `app/utils/sellSlabAction.server.ts`,
//! `app/utils/contractsBackend.server.ts` and the Remix `api.unsell.$sale`
//! route.

pub mod contract;
mod daily;
mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::get, handlers::update))
        .routes(routes!(handlers::create))
        .routes(routes!(handlers::unsell))
        .routes(routes!(contract::contract))
        .routes(routes!(daily::daily))
}
