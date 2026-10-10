//! The public showroom: catalogs anyone may browse, without a session.
//!
//! - `GET /v1/public/companies/by-subdomain/{subdomain}` → the company id
//! - `GET /v1/public/companies/{company_id}/stones` (+ `/search`, `/{id}`,
//!   `/{id}/slabs`, `/{id}/images`) → displayed stones in stock
//! - `GET /v1/public/companies/{company_id}/sinks|faucets` → displayed models in stock
//! - `GET /v1/public/colors` → stone colors
//!
//! Nothing here returns costs, hidden items or sold slabs. Replaces the
//! loaders of `customer.$company.stones|sinks|faucets`,
//! `customer.$company.stones.slabs.$stone`, `contractors.$company.stones`
//! and `customer.stones` in the Remix app.

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::company_by_subdomain))
        .routes(routes!(handlers::stones))
        .routes(routes!(handlers::search_stones))
        .routes(routes!(handlers::stone))
        .routes(routes!(handlers::stone_slabs_handler))
        .routes(routes!(handlers::stone_images))
        .routes(routes!(handlers::sinks))
        .routes(routes!(handlers::faucets))
        .routes(routes!(handlers::colors))
}
