#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::option_if_let_else, clippy::missing_errors_doc)]
use crate::crud::users::get_sales_users;
use crate::telegram::send::send_lead_manager_message;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::{
    Router,
    extract::{Json, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crud::leads::{create_lead_from_facebook, create_lead_from_wordpress};
use lambda_http::{Error, run, tracing};
use middleware::request_logger::print_request_body;
use schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use schemas::documenso::WebhookEvent;
use sqlx::MySqlPool;
use std::env::set_var;
use telegram::receive::webhook_handler;

pub mod amazon;
pub mod axum_helpers;
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
    let result = create_lead_from_wordpress(&pool, &contact_form, company_id)
        .await
        .unwrap();
    let all_users = get_sales_users(&pool, company_id).await.unwrap();
    let candidates: Vec<(String, i32)> = all_users
        .iter()
        .map(|user| (user.name.clone().unwrap(), user.id.clone()))
        .collect();
    let sales_manager = all_users.iter().find(|item| item.position_id == Some(2));
    let sales_manager_id = sales_manager.unwrap().telegram_id.clone().unwrap();
    send_lead_manager_message(
        &contact_form.to_string(),
        result.last_insert_id(),
        sales_manager_id,
        &candidates,
    )
    .await
    .unwrap();

    (StatusCode::CREATED, "created").into_response()
}

async fn facebook_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> Response {
    let result = create_lead_from_facebook(&pool, &contact_form, company_id).await;
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
        .route("/telegram/webhook", post(webhook_handler))
        .layer(axum::middleware::from_fn(print_request_body))
        .with_state(pool);

    run(app).await
}
