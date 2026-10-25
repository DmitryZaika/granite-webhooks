//! Customers.
//!
//! - `GET  /v1/customers`              → list rows (`view`, `sales_rep`, `show_invalid` query params)
//! - `GET  /v1/customers/page`         → one page of the customers table, with emails and month rows
//! - `POST /v1/customers/emails/batch` → every email of the given customers, primary first
//! - `GET  /v1/customers/search`       → customers matching a name, phone, email or company
//! - `GET  /v1/customers/{id}`         → one customer with contact details
//! - `POST /v1/customers`, `PUT /v1/customers/{id}` → create, change
//! - `PUT  /v1/customers/{id}/sales-rep`, `/temperature` → assign, rate
//! - `GET  /v1/customers/{id}/history` → deals, reassignments, emails, activities and notes
//! - `GET  /v1/customers/{id}/projects`, `PUT /v1/customers/{id}/parent` → linked projects
//! - `GET  /v1/customers/sms-contacts`, `POST /v1/customers/{id}/phone`, `POST /v1/customers/from-phone` → the SMS inbox's contacts
//!
//! Replaces `loadCustomersListPage` in the Remix app
//! (`app/utils/customersListLoader.server.ts`).

mod activity_search;
pub(crate) mod duplicates;
pub mod estimate_time;
pub(crate) mod fuzzy;
mod handlers;
pub mod history;
pub(crate) mod phones;
mod projects;
pub mod queries;
mod recipients;
pub mod schemas;
pub mod service;
pub mod signatures;
mod write;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, write::create))
        .routes(routes!(handlers::page))
        .routes(routes!(handlers::emails_batch))
        .routes(routes!(handlers::search))
        .routes(routes!(duplicates::duplicates))
        .routes(routes!(recipients::recipients))
        .routes(routes!(handlers::get, write::update, write::delete))
        .routes(routes!(write::set_sales_rep))
        .routes(routes!(write::set_temperature))
        .routes(routes!(write::mark_invalid))
        .routes(routes!(history::history))
        .routes(routes!(signatures::signature_requests))
        .routes(routes!(activity_search::activity_search))
        .routes(routes!(estimate_time::estimate_time))
        .routes(routes!(projects::projects))
        .routes(routes!(projects::set_parent))
        .routes(routes!(projects::sales))
        .routes(routes!(phones::sms_contacts))
        .routes(routes!(phones::link_phone))
        .routes(routes!(phones::from_phone))
        .routes(routes!(phones::by_phone))
}
