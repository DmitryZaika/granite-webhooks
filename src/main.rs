#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::option_if_let_else, clippy::missing_errors_doc)]
use axum::extract::Path;
use axum::http::StatusCode;
use axum::{
    extract::{Json, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use crud::leads::{create_lead_from_facebook, create_lead_from_wordpress};
use lambda_http::{run, tracing, Error};
use middleware::request_logger::print_request_body;
use schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use schemas::documenso::WebhookEvent;
use sqlx::MySqlPool;
use std::env::set_var;

pub mod crud;
pub mod middleware;
pub mod schemas;
pub mod telegram;

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn documenso(payload: Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {payload:?}");
    StatusCode::OK
}

async fn wordpress_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> Response {
    let result = create_lead_from_wordpress(&pool, &contact_form, company_id).await;
    match result {
        Ok(_) => (StatusCode::CREATED, contact_form.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn facebook_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> Response {
    let result = create_lead_from_facebook(&pool, &contact_form, company_id).await;

    match result {
        Ok(_) => (StatusCode::CREATED, contact_form.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
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
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool);

    run(app).await
}
