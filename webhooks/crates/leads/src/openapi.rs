//! The `OpenAPI` document of the lead form route (`/api-docs/openapi.json`),
//! for the integrations that post leads (`docs.granite-manager.com`).

use axum::response::IntoResponse;
// Imported, not named by path in `paths(...)`: utoipa would make the path a tag.
#[cfg(feature = "openapi")]
use crate::schemas::add_customer::NewLeadForm;
#[cfg(feature = "openapi")]
use crate::webhooks::receive::__path_new_lead_form;

#[cfg(feature = "openapi")]
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Granite Manager Webhooks API",
        description = "Lead intake for Make, Zapier, and website forms.\n\n\
For `/v1/webhooks/new-lead-form/{company_id}`:\n\
- `referral_source` is optional; when set, prefer `website` or `facebook` for statistics.\n\
- `form_name` is optional; when set, use the specific form id (e.g. `cabinet_quote`, \
`facebook_form`, `facebook_cabinet_quote_form`, `quick_quote`).\n\n\
Authenticate with the access token Granite Manager gave you: \
`Authorization: Bearer gmk_...`. Each token works for one company."
    ),
    paths(new_lead_form),
    components(schemas(NewLeadForm)),
    modifiers(&AccessTokenAuth),
    security(("access_token" = []))
)]
struct ApiDoc;

/// The `access_token` bearer scheme the docs refer to.
#[cfg(feature = "openapi")]
struct AccessTokenAuth;

#[cfg(feature = "openapi")]
impl utoipa::Modify for AccessTokenAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "access_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("An access token (`gmk_...`)."))
                    .build(),
            ),
        );
    }
}

/// The document generated from the route annotations.
#[cfg(feature = "openapi")]
pub async fn openapi_spec() -> impl IntoResponse {
    axum::Json(<ApiDoc as utoipa::OpenApi>::openapi())
}

/// The checked-in `webhooks/openapi.json` (utoipa was a twentieth of the
/// webhooks code): `make webhooks-openapi` writes it.
#[cfg(not(feature = "openapi"))]
pub async fn openapi_spec() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        include_str!("../../../openapi.json"),
    )
}

#[cfg(all(test, not(feature = "openapi")))]
mod served_spec_tests {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn the_checked_in_spec_is_served_as_json() {
        let response = super::openapi_spec().await.into_response();
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "application/json"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(spec["paths"]["/v1/webhooks/new-lead-form/{company_id}"]["post"].is_object());
    }
}

#[cfg(all(test, feature = "openapi"))]
mod openapi_tests {
    use super::ApiDoc;
    use utoipa::OpenApi;

    #[test]
    fn spec_file_is_current() {
        let generated = ApiDoc::openapi().to_pretty_json().expect("spec serializes") + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openapi.json");
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::write(&path, &generated).expect("write openapi.json");
            return;
        }
        let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            checked_in == generated,
            "webhooks/openapi.json is stale; run `make webhooks-openapi` and commit the result"
        );
    }
}
