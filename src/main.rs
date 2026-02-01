#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(
    clippy::option_if_let_else,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::missing_panics_doc
)]
use axum_helpers::axum_app::new_main_app;
use lambda_http::{Error, run};
use sqlx::MySqlPool;
use std::env::set_var;

pub mod amazon;
pub mod amazonses;
pub mod axum_helpers;
pub mod cloudtalk;
pub mod crud;
pub mod google;
pub mod libs;
pub mod middleware;
pub mod posthog;
pub mod schemas;
pub mod telegram;
pub mod tests;
pub mod webhooks;
use lambda_http::tracing;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = MySqlPool::connect(&database_url).await?;
    let app = new_main_app(pool);
    run(app).await
}
