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
    clippy::struct_excessive_bools,
    // Doc comments double as OpenAPI descriptions; product names like
    // CloudTalk are prose there, not code.
    clippy::doc_markdown
)]

pub mod auth;
pub mod error;
pub mod openapi;
pub mod routes;
pub mod serde_helpers;
pub mod state;

use axum::http::header::CONTENT_TYPE;
use axum::{Router, routing::get};
use state::AppState;
use std::sync::Arc;

async fn health_check() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    let cors = state.cors_layer();
    let (routes, spec) = openapi::router_with_spec();
    let spec: Arc<str> = spec.to_json().expect("OpenAPI spec serializes").into();
    Router::new()
        .route("/", get(health_check))
        .route(
            "/openapi.json",
            get(move || async move { ([(CONTENT_TYPE, "application/json")], spec.to_string()) }),
        )
        .merge(routes)
        .layer(cors)
        .with_state(state)
}
