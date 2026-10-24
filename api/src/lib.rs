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
pub mod call_notes;
pub mod chat;
pub mod commissions;
pub mod company_create;
pub mod db;
pub mod deliverability;
pub mod email_signature;
pub mod entitlements;
pub mod error;
pub mod extract;
pub mod followups;
pub mod images;
pub mod job_runs;
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
pub mod phone;
pub mod posthog;
pub mod posthog_autofix;
pub mod price_list_chunking;
pub mod price_lists;
pub mod push;
pub mod qbo;
pub mod quote_pricing;
pub mod ringcentral_connect;
pub mod routes;
pub mod serde_helpers;
pub mod setup_checklist;
pub mod signing_pdf;
pub mod signup;
pub mod signwell;
pub mod sql;
pub mod state;
pub mod storage;
pub mod supplier_file_processor;
pub mod telegram;
pub mod telnyx;
pub mod thumbnails;
pub mod transcribe;
pub mod voicemail;
pub mod webhooks_client;

use axum::extract::{MatchedPath, Request, State};
use axum::http::HeaderValue;
use axum::http::header::{CONTENT_TYPE, ORIGIN, X_CONTENT_TYPE_OPTIONS};
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

/// No response is ever content-sniffed into a type it was not sent as (a
/// JSON or text body read as HTML would run script on the API's origin).
async fn no_sniff(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// Every 5xx the API answers goes to `PostHog`'s Error Tracking with what
/// failed, named by its route ([`reported_path`]). The relay is left out:
/// its failures are `PostHog`'s own.
async fn report_server_errors(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    if response.status().is_server_error() && !path.starts_with("/ingest/") {
        let route = response
            .extensions()
            .get::<posthog::RouteTemplate>()
            .map(|route| route.0.as_str());
        posthog::capture_server_error(
            &state.telephony_http,
            method.as_str(),
            reported_path(route),
            route,
            response.status().as_u16(),
            response.extensions().get::<error::ErrorDetail>(),
        )
        .await;
    }
    response
}

/// The path a 5xx report names: the route's template
/// (`/v1/webhooks/telnyx/{token}/inbound`), never the path itself, whose
/// segments carry secrets and customers' numbers (the company's Telnyx
/// webhook token, a text thread's key).
fn reported_path(route: Option<&str>) -> &str {
    route.unwrap_or("(no matching route)")
}

/// Copies the matched route template into the response, because the outer
/// `report_server_errors` runs before routing and never sees `MatchedPath`.
/// Only a `route_layer` runs after routing, so this is added as one.
async fn record_route(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| posthog::RouteTemplate(matched.as_str().to_owned()));
    let mut response = next.run(request).await;
    if let Some(route) = route {
        response.extensions_mut().insert(route);
    }
    response
}

/// Wraps matched routes so a 5xx, a panic included, carries its route
/// template out to `report_server_errors`. The panic catch sits inside the
/// recording layer (a layer added later is outer), so the 500 it builds still
/// passes through `record_route`. The outer catch in `app` stays for panics
/// in the middleware itself, which have no route.
fn with_route_recording<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .route_layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
        .route_layer(middleware::from_fn(record_route))
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
    let all_routes = Router::new()
        .route("/", get(health_check))
        .route(
            "/openapi.json",
            get(move || async move { ([(CONTENT_TYPE, "application/json")], spec.to_string()) }),
        )
        .route(
            "/ingest/{*rest}",
            axum::routing::any(analytics_proxy::relay),
        )
        .merge(routes);
    with_route_recording(all_routes)
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            report_server_errors,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .layer(middleware::from_fn(no_sniff))
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    async fn fails() -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    async fn panics() -> StatusCode {
        panic!("quote has no slabs")
    }

    /// The router `app` builds, minus state: same recording layers.
    fn recorded_router() -> Router {
        with_route_recording(
            Router::new()
                .route("/v1/things/{id}", get(fails))
                .route("/v1/panics/{id}", get(panics)),
        )
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
    }

    async fn get_response(path: &str) -> Response {
        recorded_router()
            .oneshot(HttpRequest::get(path).body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn a_failing_route_carries_its_template() {
        let response = get_response("/v1/things/9").await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let route = response.extensions().get::<posthog::RouteTemplate>();
        assert_eq!(route.map(|r| r.0.as_str()), Some("/v1/things/{id}"));
    }

    #[tokio::test]
    async fn a_panicking_route_keeps_template_and_message() {
        let response = get_response("/v1/panics/9").await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let route = response.extensions().get::<posthog::RouteTemplate>();
        assert_eq!(route.map(|r| r.0.as_str()), Some("/v1/panics/{id}"));
        let detail = response.extensions().get::<error::ErrorDetail>();
        assert_eq!(
            detail.map(|d| d.message.as_str()),
            Some("quote has no slabs")
        );
    }

    #[tokio::test]
    async fn an_unmatched_path_has_no_template() {
        let response = get_response("/nowhere").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(
            response
                .extensions()
                .get::<posthog::RouteTemplate>()
                .is_none()
        );
    }

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
