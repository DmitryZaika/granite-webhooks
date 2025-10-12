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

fn new_main_app(pool: MySqlPool) -> Router {

    tracing::init_default_subscriber();

    Router::new()
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
        .with_state(pool)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;
    let app = new_main_app(pool);
    run(app).await
}


#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use sqlx::mysql::MySqlPoolOptions;
    use serde_json::json;
    use axum_test::TestServer;

    fn new_test_app(pool: MySqlPool) -> TestServer {
        let app = new_main_app(pool);
        TestServer::builder().build(app).unwrap()
    }

    fn unique_test_database_url() -> String {
        dotenvy::dotenv().ok();
        let base = std::env::var("TEST_DATABASE_URL").unwrap();
        let prefix = base.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or(base);
        format!("{}/granite_test_{}", prefix, rand::random::<u64>())
    }

    fn invalid_json_body() -> &'static str {
        r#"{
  "name": "William Brant",
  "Email": "brant_bill@yahoo.com",
  "Phone": "(317) 603-7047",
  "Address": "6105 North Cedarwood Drive",
  "Zip": "46055",
  "Remodel": "Kitchen",
  "project": "94",
  "Contacted": "Day",
  "Remove": "Not sure",
  "Improve": "Yes",
  "Sink": "I will provide my own sink",
  "Backsplash": "",
  "Stove": "Standard stove",
  "Message": "Quartz measurement 94"x48" iland counter tops approx 78"x26 and 79x26 and 39x26",
  "File": "https://granitedepotindy.com/wp-content/uploads/cf7-to-makewebhook-uploads/8667/68e6cd3341f69/file-507-17599562388614637280734145199875.jpg"
}"#
    }

    #[tokio::test]
    async fn test_invalid_json_unescaped_quotes_fails_for_now() {
        let db_url = unique_test_database_url();
        let pool = MySqlPoolOptions::new().connect_lazy(&db_url).unwrap();
        let app = new_test_app(pool);

        let response = app.post("/v1/webhooks/new-lead-form/1")
            .json(&json!(invalid_json_body()))
            .await;

        assert_eq!(response.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}