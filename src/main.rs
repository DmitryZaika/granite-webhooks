#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::option_if_let_else, clippy::missing_errors_doc)]
use axum::extract::Path;
use axum::http::StatusCode;
use axum::{
    Router,
    extract::{Json, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crud::leads::{create_lead_from_facebook, create_lead_from_wordpress};
use dashmap::DashMap;
use lambda_http::{Error, run, tracing};
use middleware::request_logger::print_request_body;
use schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use schemas::documenso::WebhookEvent;
use schemas::state::AppState;
use sqlx::MySqlPool;
use std::env::set_var;
use std::sync::Arc;
use telegram::receive::webhook_sales_button;

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
    State(app_state): State<AppState>,
    Json(contact_form): Json<WordpressContactForm>,
) -> Response {
    let result = create_lead_from_wordpress(&app_state.pool, &contact_form, company_id).await;
    // THis will send the messsage that triggers webhook_sales_button, ignore for now
    match result {
        Ok(_) => (StatusCode::CREATED, contact_form.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn facebook_contact_form(
    Path(company_id): Path<i32>,
    State(app_state): State<AppState>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> Response {
    let result = create_lead_from_facebook(&app_state.pool, &contact_form, company_id).await;
    // THis will send the messsage that triggers webhook_sales_button, ignore for now

    match result {
        Ok(_) => (StatusCode::CREATED, contact_form.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;

    tracing::init_default_subscriber();
    let app_state = AppState {
        webhook_secret: "1234567890".to_string(),
        bot: teloxide::Bot::from_env(),
        pool,
        verifications: Arc::new(DashMap::new()),
    };

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
        .route("/telegram/webhook", post(webhook_sales_button))
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(app_state);

    run(app).await
}
