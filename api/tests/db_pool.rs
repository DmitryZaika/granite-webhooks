//! The Lambda pool must not hold connections the database can't reclaim.

use api::db::lambda_pool;
use sqlx::MySql;
use sqlx::mysql::MySqlConnectOptions;
use sqlx::pool::PoolOptions;

#[sqlx::test]
async fn lambda_pool_uses_one_short_lived_connection(
    _pool_options: PoolOptions<MySql>,
    connect_options: MySqlConnectOptions,
) {
    let pool = lambda_pool(connect_options);
    assert_eq!(pool.size(), 0, "connects lazily");

    let wait_timeout: u64 = sqlx::query_scalar("SELECT @@SESSION.wait_timeout")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(wait_timeout, 60);
    assert_eq!(pool.options().get_max_connections(), 1);

    // Parallel queries share the single connection instead of deadlocking.
    let (a, b) = tokio::join!(
        sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&pool),
        sqlx::query_scalar::<_, i64>("SELECT 2").fetch_one(&pool),
    );
    assert_eq!((a.unwrap(), b.unwrap()), (1, 2));
    assert_eq!(pool.size(), 1);
}
