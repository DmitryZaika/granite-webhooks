use axum::{Router, response::IntoResponse, routing::get};
use sqlx::MySqlPool;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use webhooks_core::libs::constants::OK_RESPONSE;
use webhooks_core::middleware::request_logger::print_request_body;
// Imported, not named by path in `paths(...)`: utoipa would make the path a tag.
#[cfg(feature = "openapi")]
use webhooks_core::schemas::add_customer::NewLeadForm;
#[cfg(feature = "openapi")]
use webhooks_leads::webhooks::receive::{__path_new_lead_form, new_lead_form};

async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
}

#[cfg(feature = "openapi")]
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Granite Manager Webhooks API",
        description = "Lead intake for Make, Zapier, and website forms.\n\n\
For `/v1/webhooks/new-lead-form/{company_id}`:\n\
- `referral_source` is optional; when set, prefer `website` or `facebook` for statistics.\n\
- `form_name` is optional; when set, use the specific form id (e.g. `cabinet_quote`, \
`facebook_form`, `facebook_cabinet_quote_form`, `quick_quote`)."
    ),
    paths(new_lead_form),
    components(schemas(NewLeadForm))
)]
struct ApiDoc;

/// The document generated from the route annotations.
#[cfg(feature = "openapi")]
async fn openapi_spec() -> impl IntoResponse {
    axum::Json(<ApiDoc as utoipa::OpenApi>::openapi())
}

/// The checked-in `webhooks/openapi.json` (utoipa is a large share of this
/// crate's build): `make webhooks-openapi` writes it.
#[cfg(not(feature = "openapi"))]
async fn openapi_spec() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        include_str!("../../openapi.json"),
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
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.json");
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

pub fn new_main_app(pool: MySqlPool) -> Router {
    Router::new()
        .route("/", get(health_check))
        .merge(webhooks_leads::router())
        .merge(webhooks_email::router())
        .merge(webhooks_phone::router())
        .route("/api-docs/openapi.json", get(openapi_spec))
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::exact(
                    "https://docs.granite-manager.com".parse().unwrap(),
                ))
                .allow_methods([axum::http::Method::GET])
                .allow_headers(Any),
        )
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool)
}
