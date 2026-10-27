//! Inventory and what is sold from it: stones, slabs, sinks and faucets,
//! suppliers and their price files, transactions and sales, documents and
//! images.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    // Row structs mirror tinyint(1) columns.
    clippy::struct_excessive_bools,
    // Doc comments double as OpenAPI descriptions; product names like
    // CloudTalk are prose there, not code.
    clippy::doc_markdown,
    // Checking that every async fn's future is Send took a third of
    // clippy's time; axum and tokio::spawn require it where it matters.
    clippy::future_not_send
)]

// The crates below, at the paths their modules had in the single `api`
// crate: `crate::state::AppState` and `crate::routes::sms` resolve here too.
// Each crate re-exports the ones below it.
pub use api_crm::*;

pub mod price_list_chunking;
pub mod price_lists;
pub mod routes;
pub mod supplier_file_processor;
