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
pub mod analytics_proxy;
pub mod auth;
pub mod automation_notice;
pub mod billing;
pub mod chat;
pub mod commissions;
pub mod db;
pub mod deliverability;
pub mod email_signature;
pub mod entitlements;
pub mod error;
pub mod extract;
pub mod followups;
pub mod images;
pub mod kitchen_design;
pub mod maps;
pub mod native_delivery;
pub mod native_signing;
pub mod oauth_state;
pub mod openapi;
pub mod pdf;
pub mod pdf_fonts;
pub mod pdf_preview;
pub mod period;
pub mod posthog;
pub mod price_list_chunking;
pub mod price_lists;
pub mod qbo;
pub mod quote_pricing;
pub mod ringcentral_connect;
pub mod routes;
pub mod serde_helpers;
pub mod setup_checklist;
pub mod signing_pdf;
pub mod signwell;
pub mod sql;
pub mod state;
pub mod storage;
pub mod supplier_file_processor;
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
            .is_ok_and(|origin| state.origin_allowed(origin))
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

/// Every 5xx the API answers goes to `PostHog`'s Error Tracking with what
/// failed. The relay is left out: its failures are `PostHog`'s own.
async fn report_server_errors(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    if response.status().is_server_error() && !path.starts_with("/ingest/") {
        posthog::capture_server_error(
            &state.telephony_http,
            method.as_str(),
            &path,
            response.status().as_u16(),
            response.extensions().get::<error::ErrorDetail>(),
        )
        .await;
    }
    response
}

/// A handler that panicked answers 500 like any internal error, and the
/// panic message is kept for `report_server_errors`.
fn panic_response(panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let message = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "panic with no message".into());
    lambda_http::tracing::error!(%message, "handler panicked");
    let mut response = error::ApiError::Internal("internal error".into()).into_response();
    response.extensions_mut().insert(error::ErrorDetail {
        kind: "Panic",
        message,
    });
    response
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
        .route(
            "/ingest/{*rest}",
            axum::routing::any(analytics_proxy::relay),
        )
        .merge(routes)
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            report_server_errors,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_answers_500_and_keeps_its_message() {
        let response = panic_response(Box::new("quote has no slabs"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let detail = response
            .extensions()
            .get::<error::ErrorDetail>()
            .expect("detail");
        assert_eq!(detail.kind, "Panic");
        assert_eq!(detail.message, "quote has no slabs");

        let response = panic_response(Box::new(String::from("index out of bounds")));
        let detail = response
            .extensions()
            .get::<error::ErrorDetail>()
            .expect("detail");
        assert_eq!(detail.message, "index out of bounds");
    }
}
