use crate::amazonses::routes::{read_receipt_handler, receive_handler};
use crate::cloudtalk::receive::{sms_received, sms_sent, sync_cloudtalk};
use crate::google::receive::address_information;
use crate::libs::constants::OK_RESPONSE;
use crate::middleware::request_logger::print_request_body;
use crate::telegram::receive::webhook_handler;
use crate::template::receive::{get_complete_template, get_template_variables};
use crate::webhooks::receive::{facebook_contact_form, wordpress_contact_form};

use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use sqlx::MySqlPool;

use crate::webhooks::receive::new_lead_form;
async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
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
        .route("/ses/read-receipt", post(read_receipt_handler))
        .route("/ses/receive-email", post(receive_handler))
        .route("/cloudtalk/sms/{company_id}", post(sms_received))
        .route("/cloudtalk/sms/sent/{company_id}", post(sms_sent))
        .route(
            "/cloudtalk/sync/{company_id}/{customer_id}",
            post(sync_cloudtalk),
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
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool)
}
