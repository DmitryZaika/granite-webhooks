use crate::amazonses::routes::{read_receipt_handler, receive_handler};
use crate::libs::constants::OK_RESPONSE;
use crate::middleware::request_logger::print_request_body;
use crate::telegram::receive::webhook_handler;
use crate::webhooks::receivers::{facebook_contact_form, wordpress_contact_form};
use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use sqlx::MySqlPool;

use crate::webhooks::receivers::new_lead_form;
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
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool)
}
