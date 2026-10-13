use api::app;
use api::db::lambda_pool;
use api::state::AppState;
use lambda_http::{Error, run, tracing};
use std::env::set_var;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL must be set")?;
    let pool = lambda_pool(url.parse()?);
    let state = AppState::from_env(pool)?;
    run(app(state)).await
}
