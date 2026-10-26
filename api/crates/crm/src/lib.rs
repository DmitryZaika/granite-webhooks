//! Customers, deals and the conversations with them: texts (`routes::sms`),
//! emails, calls, Granite Phone's logic that texts and calls share (`phone`),
//! templates, uploads and the user's stats.
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
pub use api_media::*;

pub mod automation_notice;
pub mod phone;
pub mod routes;
