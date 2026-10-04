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
use axum::{
    Router,
    routing::{get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/customers", get(handlers::list))
        .route("/v1/customers/emails/batch", post(handlers::emails_batch))
}
