use api::app;
use api::state::AppState;
use common::crud::setup::create_db_pool;
use lambda_http::{Error, run, tracing};
use std::env::set_var;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let pool = create_db_pool().await?;
    let state = AppState::from_env(pool)?;
    run(app(state)).await
}
