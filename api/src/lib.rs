//! Frontend-facing JSON API for Granite Manager.
//!
//! Each route replaces one database read that a Remix loader used to perform.
//! See `api/README.md` for the migration playbook and route layout rules.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    // Row structs mirror tinyint(1) columns.
    clippy::struct_excessive_bools
)]

pub mod auth;
pub mod error;
pub mod routes;
pub mod serde_helpers;
pub mod state;

use axum::{Router, routing::get};
use state::AppState;

async fn health_check() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    let cors = state.cors_layer();
    Router::new()
        .route("/", get(health_check))
        .merge(routes::router())
        .layer(cors)
        .with_state(state)
}
