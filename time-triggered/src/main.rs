use lambda_runtime::{run, tracing, Error};
mod generic_handler;
use common::crud::setup::create_db_pool;
use generic_handler::function_handler;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();
    let pool = create_db_pool().await?;

    run(lambda_runtime::service_fn(|event| {
        let pool = pool.clone();
        async move { function_handler(&pool, event).await }
    }))
    .await?;

    Ok(())
}
