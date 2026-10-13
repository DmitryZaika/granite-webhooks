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

pub mod ai;
pub mod auth;
pub mod billing;
pub mod db;
pub mod email_signature;
pub mod error;
pub mod extract;
pub mod followups;
pub mod images;
pub mod maps;
pub mod native_delivery;
pub mod native_signing;
pub mod openapi;
pub mod pdf;
pub mod pdf_fonts;
pub mod pdf_preview;
pub mod period;
pub mod posthog;
pub mod price_lists;
pub mod qbo;
pub mod routes;
pub mod serde_helpers;
pub mod signing_pdf;
pub mod signwell;
pub mod sql;
pub mod state;
pub mod storage;
pub mod telegram;
pub mod thumbnails;
pub mod transcribe;
pub mod voicemail;
pub mod webhooks_client;

use axum::extract::{Request, State};
use axum::http::header::{CONTENT_TYPE, ORIGIN};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use state::AppState;
use std::sync::Arc;

async fn health_check() -> &'static str {
    "ok"
}

/// CSRF defence for cookie-authenticated writes: a browser sends `Origin`
/// on every cross-site request that changes something, so a write whose
/// origin is not one of the app's is refused before any handler runs.
/// Requests without `Origin` (server-to-server jobs, webhooks) pass; they
/// carry no browser cookies.
async fn check_origin(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe
        && let Some(origin) = request.headers().get(ORIGIN)
        && !origin
            .to_str()
            .is_ok_and(|origin| state::origin_allowed(&state.allowed_origins, origin))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(error::ErrorBody {
                error: "origin not allowed".into(),
            }),
        )
            .into_response();
    }
    next.run(request).await
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
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .layer(cors)
        .with_state(state)
}
