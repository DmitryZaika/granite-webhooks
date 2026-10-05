//! Customers.
//!
//! - `GET  /v1/customers`              → list rows (`view`, `sales_rep`, `show_invalid` query params)
//! - `POST /v1/customers/emails/batch` → every email of the given customers, primary first
//!
//! Replaces `loadCustomersListPage` in the Remix app
//! (`app/utils/customersListLoader.server.ts`).

mod handlers;
pub mod queries;
pub mod schemas;

use crate::state::AppState;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list))
        .routes(routes!(handlers::emails_batch))
}
