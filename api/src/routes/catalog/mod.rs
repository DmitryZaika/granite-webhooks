//! Company catalogs the inventory forms pick from.
//!
//! - `GET    /v1/suppliers`          → the company's suppliers
//! - `GET    /v1/colors`             → stone colors (shared by all companies)
//! - `GET    /v1/stone-types`        → the company's stone types
//! - `POST   /v1/stone-types`        → add a type
//! - `PATCH  /v1/stone-types/{id}`   → rename a type (and its stones)
//! - `DELETE /v1/stone-types/{id}`   → delete an unused type
//!
//! Replaces the supplier/color queries of the Remix stone forms and
//! `app/utils/stoneTypes.server.ts`.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::suppliers))
        .routes(routes!(handlers::colors))
        .routes(routes!(handlers::stone_types, handlers::create_stone_type))
        .routes(routes!(
            handlers::rename_stone_type,
            handlers::delete_stone_type
        ))
}
