use axum::http::StatusCode;
use axum::{
    extract,
    routing::post,
    Router,
};
use axum::response::IntoResponse;
use lambda_http::{run, tracing, Error};
use std::env::set_var;
use schemas::documenso::WebhookEvent;
use stripe::{Event, EventObject, EventType};

pub mod schemas;

async fn documenso(payload: extract::Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {:?}", payload);
    StatusCode::OK
}

async fn stripe(event: extract::Json<Event>) -> impl IntoResponse {
    if let EventType::CheckoutSessionCompleted = event.type_ {
        if let EventObject::CheckoutSession(session) = &event.data.object {
            println!("Оплата завершена, session id = {}", session.id);
        }
    }
    StatusCode::OK
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

    // required to enable CloudWatch error logging by the runtime
    tracing::init_default_subscriber();

    let app = Router::new()
        .route("/documenso", post(documenso))
        .route("/stripe", post(stripe));

    run(app).await
}
