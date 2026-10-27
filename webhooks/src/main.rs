use common::crud::setup::create_db_pool;
use lambda_http::{Error, tracing};
use std::env::set_var;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Here, not where it is reported: a new commit then rebuilds only this.
    common::release::set(option_env!("GIT_SHA"));
    tracing::init_default_subscriber();
    unsafe {
        set_var("AWS_LAMBDA_HTTP_IGNORE_STAGE_IN_PATH", "true");
    }
    let pool = create_db_pool().await?;
    webhooks::run(pool).await
}
