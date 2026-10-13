//! The API's MySQL pool, sized for Lambda.

use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use std::time::Duration;

/// The pool every API Lambda container uses.
///
/// - Lazy: connecting on the first query instead of at startup means a full
///   database (`Too many connections`) fails that request with a JSON 500
///   that carries CORS headers. Connecting at startup made the container
///   exit, and API Gateway's bare 502 reached the browser as "Failed to fetch".
/// - One connection: a container serves one request at a time, and every
///   Lambda container counts against the database's limit (60), so the
///   connection count tracks the number of warm containers. No handler queries
///   the pool while holding a transaction or a streaming query, and the
///   `tokio::join!`s just run one after another.
/// - Session `wait_timeout` of 60 seconds: Lambda freezes, then discards,
///   containers without closing their connections, so the pool's own idle
///   timeout never runs. The server's setting (8 hours) let those idle
///   connections fill the database's limit; this makes MySQL drop them
///   itself. A warm container whose connection was dropped reconnects on its
///   next query.
pub fn lambda_pool(options: MySqlConnectOptions) -> MySqlPool {
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(30))
        .max_lifetime(Duration::from_mins(10))
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET SESSION wait_timeout = 60")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_lazy_with(options)
}
