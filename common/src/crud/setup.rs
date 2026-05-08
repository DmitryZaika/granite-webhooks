use sqlx::mysql::MySqlPool;
use std::env;

pub async fn create_db_pool() -> Result<MySqlPool, sqlx::Error> {
    // 1. Retrieve the URL from the environment
    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL environment variable must be set");

    // 2. Establish and return the connection pool
    MySqlPool::connect(&database_url).await
}
