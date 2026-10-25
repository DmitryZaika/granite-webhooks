//! OpenAPI document for the API.
//!
//! Served at `GET /openapi.json` and checked in as `api/openapi.json`. Paths
//! and schemas come from the `#[crate::openapi::path]` and `ToSchema`
//! annotations on each route; this module only adds the document-level parts.
//!
//! utoipa only runs with the `openapi` feature (`make api-openapi` and the
//! spec tests): it is a large share of the crate's compile time. Without it
//! the annotations expand to nothing but each route's method and path,
//! [`OpenApiRouter`] registers routes straight with axum, and
//! `/openapi.json` serves the checked-in file (`make api-openapi`).

use crate::routes;
use crate::state::AppState;
use axum::Router;
use std::sync::Arc;

pub use api_macros::path;
#[cfg(not(feature = "openapi"))]
pub use api_macros::{IntoParams, ToSchema, routes};
#[cfg(not(feature = "openapi"))]
pub use plain::OpenApiRouter;
#[cfg(feature = "openapi")]
pub use utoipa::{IntoParams, ToSchema};
#[cfg(feature = "openapi")]
pub use utoipa_axum::{router::OpenApiRouter, routes};

#[cfg(feature = "openapi")]
use crate::error::ErrorBody;
#[cfg(feature = "openapi")]
use utoipa::openapi::OpenApi as OpenApiDoc;
#[cfg(feature = "openapi")]
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
#[cfg(feature = "openapi")]
use utoipa::{Modify, OpenApi};

/// Name of the `__session` cookie scheme every route lists under `security`.
pub const SESSION_SECURITY: &str = "session_cookie";

/// Name of the Granite Phone app's `X-Granite-Device` token scheme.
pub const DEVICE_SECURITY: &str = "device_token";

#[cfg(feature = "openapi")]
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Granite Manager API",
        description = "JSON API behind Granite Manager, a CRM for countertop and \
            stone fabrication companies. Every request acts as the signed-in user \
            and only sees data of that user's company."
    ),
    components(schemas(ErrorBody)),
    modifiers(&SessionCookie),
    tags(
        (name = "me", description = "The signed-in user"),
        (name = "customers", description = "Customers and leads of the company"),
        (name = "users", description = "Other users of the company"),
        (name = "stones", description = "Stones (slab materials) and their slab inventory"),
        (name = "instructions", description = "The company's how-to pages"),
        (name = "places", description = "Address suggestions from Google Places"),
        (name = "calls", description = "Calls through the company's telephony provider"),
        (name = "quotes", description = "Quote tools"),
        (name = "auth", description = "Sign-in, sign-out, password reset and the session cookie"),
        (name = "delayed", description = "Scheduled emails and texts"),
        (name = "ai-usage", description = "AI usage for the super-user dashboard"),
        (name = "ai", description = "AI helpers for writing messages"),
        (name = "public", description = "The public showroom; no session needed"),
        (name = "catalog", description = "Suppliers, colors and stone types the inventory forms pick from"),
        (name = "uploads", description = "Presigned uploads of images to storage"),
        (name = "sales", description = "Sales of slabs, sinks and faucets to customers"),
        (name = "slabs", description = "Single slabs and remnants of a stone"),
        (name = "sinks", description = "Sink models and their stock"),
        (name = "faucets", description = "Faucet models and their stock"),
        (name = "checklists", description = "Post-installation checklists installers fill in and customers sign"),
        (name = "shop", description = "The fabrication shop's own pages"),
        (name = "reports", description = "Admin reports"),
        (name = "marketing", description = "The marketing team's portal: a company's leads"),
        (name = "company", description = "The company the request acts in"),
        (name = "stats", description = "The signed-in user's own sales and messaging numbers"),
        (name = "dashboard", description = "The signed-in user's employee dashboard layout"),
        (name = "leaderboard", description = "The company's points leaderboard"),
        (name = "schedule", description = "Calendars and events of the company schedule"),
        (name = "questionnaires", description = "Sales discovery questionnaires and the answers per deal"),
        (name = "emails", description = "The caller's mailbox: folders, conversations, read state"),
        (name = "deals", description = "The sales pipeline: deals, their stages, activities and notes"),
        (name = "sms", description = "SMS conversations through CloudTalk or RingCentral"),
        (name = "notifications", description = "The notification bell"),
        (name = "quickbooks", description = "QuickBooks Online invoices and customers"),
        (name = "followups", description = "AI follow-ups: the company rule they obey, timelines and enrollments"),
        (name = "billing", description = "The company's Granite Manager subscription (Stripe)"),
        (name = "jobs", description = "Scheduled jobs the time-triggered Lambda runs (job key, no session)"),
        (name = "posthog-autofix", description = "Callbacks of the PostHog auto-fix GitHub workflow (callback secret, no session)"),
        (name = "phone", description = "Granite Phone, business calling through Telnyx: the app's sign-in and devices, calls, contacts and settings, and their admin pages"),
        (name = "telnyx-webhooks", description = "Telnyx's TeXML and Call Control callbacks for Granite Phone (signed by Telnyx, no session)"),
    )
)]
struct ApiDoc;

#[cfg(feature = "openapi")]
struct SessionCookie;

#[cfg(feature = "openapi")]
impl Modify for SessionCookie {
    fn modify(&self, openapi: &mut OpenApiDoc) {
        // utoipa fills `license` from Cargo.toml, where the crate has none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            SESSION_SECURITY,
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "__session",
                "Signed React Router session cookie set by the Granite Manager login.",
            ))),
        );
        components.add_security_scheme(
            "job_key",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "Authorization",
                "`LAMBDA_KEY`, sent by the time-triggered Lambda.",
            ))),
        );
        let mut callback = Http::new(HttpAuthScheme::Bearer);
        callback.description = Some(
            "`POSTHOG_AUTOFIX_CALLBACK_SECRET`, sent by the posthog-autofix GitHub workflow."
                .into(),
        );
        components.add_security_scheme("autofix_callback", SecurityScheme::Http(callback));
        components.add_security_scheme(
            DEVICE_SECURITY,
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-Granite-Device",
                "Device token of a signed-in Granite Phone app (`POST /v1/auth/device-sessions`).",
            ))),
        );
        let mut runner = Http::new(HttpAuthScheme::Bearer);
        runner.description = Some("`AGENT_RUNNER_KEY`, sent by the claude-job-runner.".into());
        components.add_security_scheme("agent_runner_key", SecurityScheme::Http(runner));
    }
}

/// Every route plus the OpenAPI document describing them.
#[cfg(feature = "openapi")]
pub fn router_with_spec() -> (Router<AppState>, OpenApiDoc) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(routes::router())
        .split_for_parts()
}

#[cfg(feature = "openapi")]
pub fn spec() -> OpenApiDoc {
    router_with_spec().1
}

/// Every route, and the OpenAPI document as JSON for `GET /openapi.json`.
#[cfg(feature = "openapi")]
pub fn router_and_spec_json() -> (Router<AppState>, Arc<str>) {
    let (router, spec) = router_with_spec();
    (
        router,
        spec.to_json().expect("OpenAPI spec serializes").into(),
    )
}

/// Every route, and the checked-in OpenAPI document for `GET /openapi.json`.
#[cfg(not(feature = "openapi"))]
pub fn router_and_spec_json() -> (Router<AppState>, Arc<str>) {
    (routes::router().into_router(), CHECKED_IN.into())
}

/// `api/openapi.json`, which `make api-openapi` writes.
pub const CHECKED_IN: &str = include_str!("../openapi.json");

#[cfg(not(feature = "openapi"))]
mod plain {
    use axum::Router;
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use axum::routing::{MethodRouter, Route};
    use std::convert::Infallible;
    use tower_layer::Layer;
    use tower_service::Service;

    /// `utoipa_axum`'s `OpenApiRouter` without the document: the routes of
    /// [`api_macros::routes!`] go straight to axum.
    pub struct OpenApiRouter<S = ()>(Router<S>);

    impl<S: Clone + Send + Sync + 'static> OpenApiRouter<S> {
        pub fn new() -> Self {
            Self(Router::new())
        }

        /// Registers the handlers of one `routes!`, which share a path.
        #[must_use]
        pub fn routes<const N: usize>(self, routes: [(&'static str, MethodRouter<S>); N]) -> Self {
            debug_assert!(
                routes.iter().all(|(path, _)| *path == routes[0].0),
                "the handlers of one routes! must share a path"
            );
            Self(
                routes
                    .into_iter()
                    .fold(self.0, |axum_router, (path, method_router)| {
                        axum_router.route(if path.is_empty() { "/" } else { path }, method_router)
                    }),
            )
        }

        #[must_use]
        pub fn merge(self, other: Self) -> Self {
            Self(self.0.merge(other.0))
        }

        #[must_use]
        pub fn layer<L>(self, layer: L) -> Self
        where
            L: Layer<Route> + Clone + Send + Sync + 'static,
            L::Service: Service<Request> + Clone + Send + Sync + 'static,
            <L::Service as Service<Request>>::Response: IntoResponse + 'static,
            <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
            <L::Service as Service<Request>>::Future: Send + 'static,
        {
            Self(self.0.layer(layer))
        }

        pub fn into_router(self) -> Router<S> {
            self.0
        }
    }

    impl<S: Clone + Send + Sync + 'static> Default for OpenApiRouter<S> {
        fn default() -> Self {
            Self::new()
        }
    }
}
