#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::option_if_let_else,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc
)]
use axum::{
    Router,
    response::IntoResponse,
    routing::{get, post},
};
use lambda_http::{Error, run, tracing};
use libs::constants::OK_RESPONSE;
use middleware::request_logger::print_request_body;
use sqlx::MySqlPool;
use std::env::set_var;
use telegram::receive::webhook_handler;
use webhooks::receivers::{documenso, facebook_contact_form, wordpress_contact_form};

use crate::webhooks::receivers::new_lead_form;

pub mod amazon;
pub mod axum_helpers;
pub mod crud;
pub mod google;
pub mod libs;
pub mod middleware;
pub mod posthog;
pub mod schemas;
pub mod telegram;
pub mod webhooks;

async fn health_check() -> impl IntoResponse {
    OK_RESPONSE
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;

    tracing::init_default_subscriber();

    let app = Router::new()
        .route("/", get(health_check))
        .route("/documenso", post(documenso))
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
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool);

    run(app).await
}
