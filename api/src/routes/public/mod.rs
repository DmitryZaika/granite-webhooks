//! The public showroom: catalogs anyone may browse, without a session.
//!
//! - `GET /v1/public/companies?ids=` → names and logos for the company chooser
//! - `GET /v1/public/companies/by-subdomain/{subdomain}` → the company id
//! - `GET /v1/public/companies/{company_id}/stones` (+ `/search`, `/{id}`,
//!   `/{id}/slabs`, `/{id}/images`) → displayed stones in stock
//! - `GET /v1/public/companies/{company_id}/sinks|faucets` → displayed models in stock
//! - `GET /v1/public/colors` → stone colors
//! - `GET /v1/public/vcard` → a contact card file
//! - `GET /v1/public/hold-music/{file}` → a built-in Granite Phone hold music MP3
//! - `GET /v1/public/calendly-url` → the demo booking page
//! - `GET /v1/public/companies/{company_id}/survey`, `POST .../surveys` →
//!   the post-installation survey
//!
//! Nothing here returns costs, hidden items or sold slabs. Replaces the
//! loaders of `customer.$company.stones|sinks|faucets`,
//! `customer.$company.stones.slabs.$stone`, `contractors.$company.stones`
//! `customer.stones`, `customers.companies` and `customer.$company.survey`
//! in the Remix app.

mod calendly;
mod handlers;
mod hold_music;
pub mod queries;
pub mod schemas;
mod survey;
mod vcard;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::companies))
        .routes(routes!(handlers::company_by_subdomain))
        .routes(routes!(handlers::stones))
        .routes(routes!(handlers::search_stones))
        .routes(routes!(handlers::stone))
        .routes(routes!(handlers::stone_slabs_handler))
        .routes(routes!(handlers::stone_images))
        .routes(routes!(handlers::sinks))
        .routes(routes!(handlers::faucets))
        .routes(routes!(handlers::colors))
        .routes(routes!(vcard::vcard))
        .routes(routes!(hold_music::hold_music_track))
        .routes(routes!(calendly::calendly_url))
        .routes(routes!(survey::survey_form))
        .routes(routes!(survey::create_survey))
}
