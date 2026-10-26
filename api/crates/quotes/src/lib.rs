//! Quotes: pricing (`quote_pricing`), the editor, online quotes and the
//! customer portal, signatures, QuickBooks invoices and commissions.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    // Row structs mirror tinyint(1) columns.
    clippy::struct_excessive_bools,
    // Doc comments double as OpenAPI descriptions; product names like
    // CloudTalk are prose there, not code.
    clippy::doc_markdown
)]

// The crates below, at the paths their modules had in the single `api`
// crate: `crate::state::AppState` and `crate::routes::sms` resolve here too.
// Each crate re-exports the ones below it.
pub use api_crm::*;

pub mod commissions;
pub mod quote_pricing;
pub mod routes;
