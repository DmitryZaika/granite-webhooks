//! Customers.
//!
//! - `GET  /v1/customers`              → list rows (`view`, `sales_rep`, `show_invalid` query params)
//! - `POST /v1/customers/emails/batch` → every email of the given customers, primary first
//! - `GET  /v1/customers/search`       → customers matching a name, phone, email or company
//! - `GET  /v1/customers/{id}`         → one customer with contact details
//! - `POST /v1/customers`, `PUT /v1/customers/{id}` → create, change
//! - `PUT  /v1/customers/{id}/sales-rep`, `/temperature` → assign, rate
//! - `GET  /v1/customers/{id}/history` → deals, reassignments, emails, activities and notes
//! - `GET  /v1/customers/{id}/projects`, `PUT /v1/customers/{id}/parent` → linked projects
//!
//! Replaces `loadCustomersListPage` in the Remix app
//! (`app/utils/customersListLoader.server.ts`).

mod fuzzy;
mod handlers;
mod history;
mod projects;
pub mod queries;
pub mod schemas;
pub mod service;
mod write;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, write::create))
        .routes(routes!(handlers::emails_batch))
        .routes(routes!(handlers::search))
        .routes(routes!(handlers::get, write::update))
        .routes(routes!(write::set_sales_rep))
        .routes(routes!(write::set_temperature))
        .routes(routes!(history::history))
        .routes(routes!(projects::projects))
        .routes(routes!(projects::set_parent))
}
