//! Faucet models and their stock.
//!
//! - `GET /v1/faucets` → faucet models with free unit counts (`type`,
//!   `supplier_id`, `include_sold_out` query params)
//! - `GET/PUT/DELETE /v1/faucets/{id}`, `POST /v1/faucets` → one model, admin writes (units, image)
//! - `GET/POST /v1/faucets/{id}/images`, `DELETE /v1/faucets/{id}/images/{image_id}` → installed photos
//!
//! Replaces the loader of the Remix `employee.faucets.tsx` route
//! (`faucetQueryBuilder` in `app/utils/queries.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, handlers::create))
        .routes(routes!(handlers::get, handlers::update, handlers::delete))
        .routes(routes!(handlers::images, handlers::add_images))
        .routes(routes!(handlers::delete_image))
}
