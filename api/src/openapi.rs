//! OpenAPI document for the API.
//!
//! Served at `GET /openapi.json` and checked in as `api/openapi.json`. Paths
//! and schemas come from the `#[utoipa::path]` and `ToSchema` annotations on
//! each route; this module only adds the document-level parts.

use crate::error::ErrorBody;
use crate::routes;
use crate::state::AppState;
use axum::Router;
use utoipa::openapi::OpenApi as OpenApiDoc;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;

/// Name of the `__session` cookie scheme every route lists under `security`.
pub const SESSION_SECURITY: &str = "session_cookie";

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
    )
)]
struct ApiDoc;

struct SessionCookie;

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
    }
}

/// Every route plus the OpenAPI document describing them.
pub fn router_with_spec() -> (Router<AppState>, OpenApiDoc) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(routes::router())
        .split_for_parts()
}

pub fn spec() -> OpenApiDoc {
    router_with_spec().1
}
