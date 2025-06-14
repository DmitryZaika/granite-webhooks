use axum::http::StatusCode;
use axum::{
    extract::{
        State, Json
    },
    routing::post,
    Router,
    response::{IntoResponse, Response}
};
use lambda_http::{run, tracing, Error};
use std::env::set_var;
use schemas::documenso::WebhookEvent;
use stripe::{Event, EventObject, EventType};
use sqlx::{MySqlPool, query};

pub mod schemas;

async fn documenso(payload: Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {:?}", payload);
    StatusCode::OK
}

pub async fn stripe(
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

    let app = Router::new()
        .route("/documenso", post(documenso))
        .route("/stripe", post(stripe))
        .with_state(pool);

    run(app).await
}
