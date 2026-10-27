//! Invoices the CRM keeps itself, for companies without QuickBooks.
//!
//! QuickBooks invoices are in `routes/quickbooks`. These are numbered per
//! company, made from a quote or by hand; the browser prints the PDF and
//! emails it with `POST /v1/emails`.
//!
//! - `GET /v1/invoices` → the company's invoices and the next number
//! - `POST /v1/invoices` → a new draft invoice
//! - `GET /v1/invoices/{id}` → one invoice
//! - `PUT /v1/invoices/{id}` → edit it
//! - `PUT /v1/invoices/{id}/status` → mark it sent, paid, void or draft

mod handlers;
pub mod queries;
pub mod schemas;
pub mod validate;

use crate::openapi::{OpenApiRouter, routes};
use crate::state::AppState;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(handlers::list, handlers::create))
        .routes(routes!(handlers::get, handlers::update))
        .routes(routes!(handlers::status))
}
