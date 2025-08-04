use axum::http::{StatusCode, Request, Method, Uri};
use axum::{
    extract::{
        State, Json
    },
    routing::{get, post},
    Router,
    response::{IntoResponse, Response},
    middleware::{self, Next}
};
use lambda_http::{run, tracing, Error};
use std::env::set_var;
use schemas::documenso::WebhookEvent;
use schemas::add_customer::WordpressContactForm;
use stripe::{Event, EventObject, EventType};
use sqlx::{MySqlPool, query};

pub mod schemas;

async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

async fn documenso(payload: Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {:?}", payload);
    StatusCode::OK
}

async fn stripe(
    State(pool): State<MySqlPool>,
    Json(event): Json<Event>,
) -> Response {
    let session = match (&event.type_, &event.data.object) {
        (EventType::PaymentIntentSucceeded, EventObject::PaymentIntent(s)) => s,
        _ => return (StatusCode::ACCEPTED, "ignored").into_response(),
    };


    let sale_id = match session.metadata.get("saleId") {
        Some(id) => id.as_str(),
        None => return (StatusCode::UNPROCESSABLE_ENTITY, "saleId missing").into_response(),
    };

    let result = query!(
        r#"INSERT INTO stripe_payments
           (sale_id, stripe_payment_intent_id, amount_total)
           VALUES (?, ?, ?)"#,
        sale_id,
        session.id.as_str(),
        session.amount
    )
    .execute(&pool)
    .await;

    // всегда возвращаем текст
    match result {
        Ok(_)  => (StatusCode::CREATED,               "created").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn wordpress_contact_form(
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> Response {
    let result = query!(
        r#"INSERT INTO customers
           (name, email, phone, postal_code, company_id)
           VALUES (?, ?, ?, ?, ?)"#,
        contact_form.your_name,
        contact_form.your_email,
        contact_form.phone,
        contact_form.your_zip,
        1
    )
    .execute(&pool)
    .await;

    match result {
        Ok(_)  => (StatusCode::CREATED, "created").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn logging_middleware(
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();
    
    // Log the incoming request with both full URI and routing path
    tracing::info!("Incoming request: {} {} (routing path: {})", method, uri, path);
    
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
    let database_url = std::env::var("DATABASE_URL")?;
    let pool = MySqlPool::connect(&database_url).await?;

    // required to enable CloudWatch error logging by the runtime
    tracing::init_default_subscriber();

    let lambda_routes = Router::new()
        .route("/", get(health_check))
        .route("/documenso", post(documenso))
        .route("/stripe", post(stripe))
        .route("/wordpress-contact-form", post(wordpress_contact_form));

    let app = Router::new()
        .nest("/granite-webhooks", lambda_routes)
        .layer(middleware::from_fn(logging_middleware))
        .with_state(pool);

    run(app).await
}
