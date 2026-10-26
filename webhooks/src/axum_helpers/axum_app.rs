use crate::amazonses::routes::{read_receipt_handler, receive_handler};
use crate::cloudtalk::receive::{
    call_received as cloudtalk_call_received, sms_received as cloudtalk_sms_received,
    sms_received_unscoped as cloudtalk_sms_received_unscoped, sms_sent as cloudtalk_sms_sent,
    sync_cloudtalk, unsync_cloudtalk,
};
use crate::google::receive::address_information;
use crate::libs::constants::OK_RESPONSE;
use crate::middleware::request_logger::print_request_body;
use crate::outbound::send_handler as send_email;
use crate::outbound::sms::send_handler as send_sms;
use crate::ringcentral::receive::{
    call_received as ringcentral_call_received, sms_notification as ringcentral_sms_notification,
    sms_received as ringcentral_sms_received, sms_sent as ringcentral_sms_sent, sync_ringcentral,
    unsync_ringcentral,
};
use crate::telegram::cleanup::delete_lead_telegram_messages;
use crate::telegram::crm_notify::crm_notify_handler;
use crate::telegram::notifications_notify::notifications_notify_handler;
use crate::telegram::receive::webhook_handler;
use crate::telnyx::receive::event_progress;
use crate::template::receive::{get_complete_template, get_template_variables};
use crate::webhooks::receive::{facebook_contact_form, new_lead_form, wordpress_contact_form};
use axum::{
    Router,
    response::IntoResponse,
    routing::{delete, get, post},
};
use sqlx::MySqlPool;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

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
    paths(crate::webhooks::receive::new_lead_form),
    components(schemas(crate::schemas::add_customer::NewLeadForm))
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
        .route(
            "/wordpress-contact-form/{company_id}",
            post(wordpress_contact_form),
        )
        .route(
            "/facebook-contact-form/{company_id}",
            post(facebook_contact_form),
        )
        .route(
            "/v1/webhooks/new-lead-form/{company_id}",
            post(new_lead_form),
        )
        .route("/telegram/webhook", post(webhook_handler))
        .route(
            "/telegram/lead-messages/{company_id}/{customer_id}",
            delete(delete_lead_telegram_messages),
        )
        .route("/telegram/crm-notify", post(crm_notify_handler))
        .route(
            "/telegram/notifications-notify",
            post(notifications_notify_handler),
        )
        .route("/ses/read-receipt", post(read_receipt_handler))
        .route("/ses/receive-email", post(receive_handler))
        .route("/email/send", post(send_email))
        .route("/sms/send", post(send_sms))
        .route("/cloudtalk/sms", post(cloudtalk_sms_received_unscoped))
        .route("/cloudtalk/sms/{company_id}", post(cloudtalk_sms_received))
        .route("/cloudtalk/sms/sent/{company_id}", post(cloudtalk_sms_sent))
        .route(
            "/cloudtalk/call/{company_id}",
            post(cloudtalk_call_received),
        )
        .route(
            "/cloudtalk/sync/{company_id}/{customer_id}",
            post(sync_cloudtalk).delete(unsync_cloudtalk),
        )
        .route(
            "/ringcentral/sms/{company_id}",
            post(ringcentral_sms_received),
        )
        .route(
            "/ringcentral/sms/sent/{company_id}",
            post(ringcentral_sms_sent),
        )
        .route(
            "/ringcentral/notify/{company_id}",
            post(ringcentral_sms_notification),
        )
        .route(
            "/ringcentral/call/{company_id}",
            post(ringcentral_call_received),
        )
        .route(
            "/ringcentral/sync/{company_id}/{customer_id}",
            post(sync_ringcentral).delete(unsync_ringcentral),
        )
        .route("/telnyx/event-progress", post(event_progress))
        .route(
            "/template/variables/{company_id}/{user_id}",
            get(get_template_variables),
        )
        .route(
            "/template/complete/{company_id}/{user_id}",
            post(get_complete_template),
        )
        .route("/google/address-autocomplete", post(address_information))
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
