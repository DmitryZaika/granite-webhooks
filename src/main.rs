use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::{
    extract::{Json, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use lambda_http::{run, tracing, Error};
use schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use schemas::documenso::WebhookEvent;
use sqlx::{query, MySqlPool};
use std::env::set_var;

pub mod schemas;

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn documenso(payload: Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {:?}", payload);
    StatusCode::OK
}

async fn wordpress_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> Response {
    let result = query!(
        r#"INSERT INTO customers
           (name, email, phone, postal_code, address, remodal_type, project_size, contact_time, remove_and_dispose, improve_offer, sink, company_id, referral_source, source)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
        company_id,
        "wordpress-form",
        "leads"
    )
    .execute(&pool)
    .await;

    match result {
        Ok(_) => (StatusCode::CREATED, "created").into_response(),
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
           (name,  phone, when_start, details, email, city,  postal_code, compaign_name, adset_name, ad_name, company_id, referral_source, source)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        contact_form.name,
        contact_form.phone,
        contact_form.when_start,
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
        Ok(_) => (StatusCode::CREATED, "created").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn logging_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();

    // Log the incoming request with both full URI and routing path
    tracing::info!(
        "Incoming request: {} {} (routing path: {})",
        method,
        uri,
        path
    );

    let response = next.run(request).await;

    // Optionally log the response status
    tracing::info!("Response status: {}", response.status());

    response
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    // If you use API Gateway stages, the Rust Runtime will include the stage name
    // as part of the path that your application receives.
    // Setting the following environment variable, you can remove the stage from the path.
    // This variable only applies to API Gateway stages,
    // you can remove it if you don't use them.
    // i.e with: `GET /test-stage/todo/id/123` without: `GET /todo/id/123`
    set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;

    // required to enable CloudWatch error logging by the runtime
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
        .layer(middleware::from_fn(logging_middleware))
        .with_state(pool);

    run(app).await
}
