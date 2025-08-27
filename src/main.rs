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
use lambda_http::{run, tracing, Error};
use middleware::request_logger::print_request_body;
use schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use schemas::documenso::WebhookEvent;
use sqlx::{query, MySqlPool};
use std::env::set_var;

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
    let result = query!(
        r#"INSERT INTO customers
           (name, email, phone, postal_code, address, remodal_type, project_size, contact_time, remove_and_dispose, improve_offer, sink, backsplash, kitchen_stove, your_message, attached_file, company_id, referral_source, source)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        contact_form.name,
        contact_form.email,
        contact_form.phone,
        contact_form.postal_code,
        contact_form.address,
        contact_form.remodal_type,
        contact_form.project_size,
        contact_form.contact_time,
        contact_form.remove_and_dispose,
        contact_form.improve_offer,
        contact_form.sink,
        contact_form.backsplash,
        contact_form.kitchen_stove,
        contact_form.your_message,
        contact_form.attached_file,
        company_id,
        "wordpress-form",
        "leads"
    )
    .execute(&pool)
    .await;

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
    let result = query!(
        r#"INSERT INTO customers
           (name, phone, remove_and_dispose, details, email, city, postal_code, compaign_name, adset_name, ad_name, company_id, referral_source, source)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        contact_form.name,
        contact_form.phone,
        contact_form.remove_and_dispose,
        contact_form.details,
        contact_form.email,
        contact_form.city,
        contact_form.postal_code,
        contact_form.compaign_name,
        contact_form.adset_name,
        contact_form.ad_name,
        company_id,
        "facebook-form",
        "leads"
    )
    .execute(&pool)
    .await;

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
