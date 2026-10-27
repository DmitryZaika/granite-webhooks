//! Sink models and their stock.
//!
//! - `GET /v1/sinks` → sink models with free unit counts (`type`, `supplier_id`,
//!   `include_sold_out` query params)
//! - `GET/PUT/DELETE /v1/sinks/{id}`, `POST /v1/sinks` → one model, admin writes (units, image)
//! - `GET/POST /v1/sinks/{id}/images`, `DELETE /v1/sinks/{id}/images/{image_id}` → installed photos
//!
//! Replaces the loader of the Remix `employee.sinks.tsx` route
//! (`sinkQueryBuilder` in `app/utils/queries.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, handlers::create))
        .routes(routes!(handlers::get, handlers::update, handlers::delete))
        .routes(routes!(handlers::images, handlers::add_images))
        .routes(routes!(handlers::delete_image))
}
