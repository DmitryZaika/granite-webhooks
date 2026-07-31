use sqlx::mysql::{MySqlPool, MySqlPoolOptions};
use std::env;
use std::time::Duration;

pub async fn create_db_pool() -> Result<MySqlPool, sqlx::Error> {
    // 1. Retrieve the URL from the environment
    let database_url =
        env::var("DATABASE_URL").expect("DATABASE_URL environment variable must be set");

    // 2. Establish and return the connection pool.
    //    sqlx defaults to 10 connections per pool; a warm Lambda container holds
    //    them open, so concurrent containers multiply straight into the RDS
    //    connection limit. A Lambda invocation is single-request, so 3 is ample,
    //    and the idle/lifetime caps release connections a frozen container would
    //    otherwise pin indefinitely.
    MySqlPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(60))
        .max_lifetime(Duration::from_secs(600))
        .connect(&database_url)
        .await
}
