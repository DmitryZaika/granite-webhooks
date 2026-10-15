//! Stones (materials the company sells) and their slab inventory.
//!
//! - `GET /v1/stones` → displayed stones with slab counts (`type`, `supplier_id`,
//!   `color_id`, `level`, `finishing`, `include_sold_out` query params)
//! - `GET /v1/stones/search` → stones by name, best match first
//! - `POST /v1/stones/quick` → add a stone with its first slabs
//! - `GET /v1/stones/{id}` → one stone with its color ids
//! - `GET /v1/stones/{id}/slabs` → its slabs (own + linked) with sale details
//! - `PATCH /v1/stones/{id}/samples` → samples on hand and how many to keep
//!
//! Replaces the loader of the Remix `employee.stones.tsx` route
//! (`stoneQueryBuilder` in `app/utils/queries.server.ts`).

mod handlers;
mod media;
pub mod queries;
pub mod schemas;
mod writes;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, writes::create))
        .routes(routes!(handlers::search))
        .routes(routes!(handlers::quick_add))
        .routes(routes!(handlers::get, writes::update, writes::delete))
        .routes(routes!(handlers::slabs, media::add_slab))
        .routes(routes!(writes::update_samples))
        .routes(routes!(media::names))
        .routes(routes!(media::image_proxy))
        .routes(routes!(media::links))
        .routes(routes!(media::images, media::add_images))
        .routes(routes!(media::delete_image))
        .routes(routes!(media::link_images))
        .routes(routes!(media::unlink_images))
        .routes(routes!(media::link_slabs))
        .routes(routes!(media::unlink_slabs))
}
