//! The company the request acts in.
//!
//! - `GET /v1/company/features` → what the company has switched on or set up
//! - `GET /v1/company/drive-miles` → driving miles from the company to an address
//! - `GET /v1/company/tax-rate` → the state sales tax

mod handlers;
pub mod queries;
pub mod schemas;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::features))
        .routes(routes!(handlers::drive_miles))
        .routes(routes!(handlers::tax_rate))
}
