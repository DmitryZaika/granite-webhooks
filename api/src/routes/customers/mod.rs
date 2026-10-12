//! Customers.
//!
//! - `GET  /v1/customers`              → list rows (`view`, `sales_rep`, `show_invalid` query params)
//! - `POST /v1/customers/emails/batch` → every email of the given customers, primary first
//! - `GET  /v1/customers/search`       → customers matching a name, phone, email or company
//! - `GET  /v1/customers/{id}`         → one customer with contact details
//! - `GET  /v1/customers/{id}/history` → deals, reassignments, emails, activities and notes
//!
//! Replaces `loadCustomersListPage` in the Remix app
//! (`app/utils/customersListLoader.server.ts`).

mod fuzzy;
mod handlers;
mod history;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list))
        .routes(routes!(handlers::emails_batch))
        .routes(routes!(handlers::search))
        .routes(routes!(handlers::get))
        .routes(routes!(history::history))
}
