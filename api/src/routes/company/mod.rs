//! The company the request acts in.
//!
//! - `GET /v1/company/features` → what the company has switched on or set up
//! - `GET /v1/company/drive-miles` → driving miles from the company to an address

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::features))
        .routes(routes!(handlers::drive_miles))
}
