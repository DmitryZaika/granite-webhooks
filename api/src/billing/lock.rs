//! One company's billing lock: a MySQL named lock around each read-modify-write
//! of its `company_billing` row (Stripe sends related events at the same
//! moment, and each one rewrites the whole row).
//!
//! The lock lives on its own connection, opened with the pool's options and
//! closed after: the Lambda pool has a single connection, which the locked work
//! itself needs. Dropping a held lock closes that connection, and MySQL then
//! releases the lock. Not re-entrant: take it at the entry points only (webhook
//! apply, gate settle, sync, checkout, plan change, reactivation).

use sqlx::{Connection, MySqlConnection, MySqlPool};
use std::future::Future;

/// How long to wait for another request holding the lock.
pub const LOCK_TIMEOUT_SECS: u32 = 10;
pub const BUSY_MESSAGE: &str = "Billing is busy right now. Please try again.";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another request held the lock for the whole timeout.
    #[error("{BUSY_MESSAGE}")]
    Busy,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// `stripe_billing:<company id>:<database>`: the database too, so databases
/// on one server (local, tests) do not share locks. MySQL caps names at 64.
const LOCK_NAME: &str = "LEFT(CONCAT('stripe_billing:', ?, ':', COALESCE(DATABASE(), '')), 64)";

/// A held company lock.
pub struct CompanyLock {
    conn: MySqlConnection,
    company_id: i32,
}

impl CompanyLock {
    pub async fn acquire(pool: &MySqlPool, company_id: i32) -> Result<Self, LockError> {
        Self::acquire_within(pool, company_id, LOCK_TIMEOUT_SECS).await
    }

    pub async fn acquire_within(
        pool: &MySqlPool,
        company_id: i32,
        timeout_secs: u32,
    ) -> Result<Self, LockError> {
        let mut conn = MySqlConnection::connect_with(&pool.connect_options()).await?;
        let acquired: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT GET_LOCK({LOCK_NAME}, ?)"
        )))
        .bind(company_id)
        .bind(timeout_secs)
        .fetch_one(&mut conn)
        .await?;
        if acquired != Some(1) {
            let _ = conn.close().await;
            return Err(LockError::Busy);
        }
        Ok(Self { conn, company_id })
    }

    /// Releases the lock on the connection that took it, and closes it.
    pub async fn release(mut self) {
        let released = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT RELEASE_LOCK({LOCK_NAME})"
        )))
        .bind(self.company_id)
        .execute(&mut self.conn)
        .await;
        if let Err(err) = released {
            lambda_http::tracing::warn!(error = %err, "billing lock release failed");
        }
        let _ = self.conn.close().await;
    }
}

/// Runs `work` while holding the company's lock.
pub async fn with_company_lock<T, E, F>(pool: &MySqlPool, company_id: i32, work: F) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    E: From<LockError>,
{
    let lock = CompanyLock::acquire(pool, company_id).await?;
    let result = work.await;
    lock.release().await;
    result
}
