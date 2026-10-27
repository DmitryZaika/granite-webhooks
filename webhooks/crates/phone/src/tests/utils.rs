pub use webhooks_core::tests::utils::*;

use axum_test::TestServer;
use sqlx::MySqlPool;

/// This crate's routes on `pool`.
pub fn new_test_app(pool: MySqlPool) -> TestServer {
    TestServer::builder().build(crate::router().with_state(pool))
}
