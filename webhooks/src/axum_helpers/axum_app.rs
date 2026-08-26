use crate::amazonses::routes::{read_receipt_handler, receive_handler};
use crate::cloudtalk::receive::{
    call_received as cloudtalk_call_received, sms_received as cloudtalk_sms_received,
    sms_sent as cloudtalk_sms_sent, sync_cloudtalk,
};
use crate::ringcentral::receive::{
    call_received as ringcentral_call_received, sms_received as ringcentral_sms_received,
    sms_sent as ringcentral_sms_sent, sync_ringcentral,
};
use crate::google::receive::address_information;
use crate::libs::constants::OK_RESPONSE;
use crate::middleware::request_logger::print_request_body;
use crate::schemas::add_customer::NewLeadForm;
use crate::telegram::cleanup::delete_lead_telegram_messages;
use crate::telegram::crm_notify::crm_notify_handler;
use crate::telegram::notifications_notify::notifications_notify_handler;
use crate::telegram::receive::webhook_handler;
use crate::template::receive::{get_complete_template, get_template_variables};
use crate::webhooks::receive::{
    __path_new_lead_form, facebook_contact_form, new_lead_form, wordpress_contact_form,
};
use axum::{
    Json, Router,
    response::IntoResponse,
    routing::{delete, get, post},
};
use sqlx::MySqlPool;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use utoipa::OpenApi;

async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
}

#[derive(OpenApi)]
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

async fn openapi_spec() -> impl IntoResponse {
    Json(ApiDoc::openapi())
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
        .route("/cloudtalk/sms/{company_id}", post(cloudtalk_sms_received))
        .route("/cloudtalk/sms/sent/{company_id}", post(cloudtalk_sms_sent))
        .route("/cloudtalk/call/{company_id}", post(cloudtalk_call_received))
        .route(
            "/cloudtalk/sync/{company_id}/{customer_id}",
            post(sync_cloudtalk),
        )
        .route("/ringcentral/sms/{company_id}", post(ringcentral_sms_received))
        .route(
            "/ringcentral/sms/sent/{company_id}",
            post(ringcentral_sms_sent),
        )
        .route("/ringcentral/call/{company_id}", post(ringcentral_call_received))
        .route(
            "/ringcentral/sync/{company_id}/{customer_id}",
            post(sync_ringcentral),
        )
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
