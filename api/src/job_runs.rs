//! `job_runs`: throttles for scheduled jobs that run less often than the
//! one-minute tick.
//!
//! Lambda containers keep no memory between ticks, so each job stamps its
//! start in `job_runs`. The row is locked while it is read and stamped, so of
//! two overlapping ticks exactly one claims the run.

use chrono::{Duration, NaiveDateTime};
use sqlx::MySqlPool;

/// A job run this tick won; `finish` or `undo` it when the work is over.
#[derive(Debug)]
pub struct Claim {
    key: String,
    started_at: NaiveDateTime,
    previous: Option<NaiveDateTime>,
}

/// Takes the run of `key` when it last started at least `every` ago (or
/// never); `None` when it is not due or another tick took it.
pub async fn claim(
    pool: &MySqlPool,
    key: &str,
    every: Duration,
    now: NaiveDateTime,
) -> Result<Option<Claim>, sqlx::Error> {
    // Outside the transaction: two inserts of one key inside one would
    // deadlock on the shared lock a duplicate takes.
    sqlx::query("INSERT IGNORE INTO job_runs (job_key) VALUES (?)")
        .bind(key)
        .execute(pool)
        .await?;
    let mut tx = pool.begin().await?;
    let previous: Option<NaiveDateTime> =
        sqlx::query_scalar("SELECT last_started_at FROM job_runs WHERE job_key = ? FOR UPDATE")
            .bind(key)
            .fetch_one(&mut *tx)
            .await?;
    if previous.is_some_and(|at| at > now - every) {
        tx.commit().await?;
        return Ok(None);
    }
    sqlx::query("UPDATE job_runs SET last_started_at = ? WHERE job_key = ?")
        .bind(now)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(Claim {
        key: key.to_string(),
        started_at: now,
        previous,
    }))
}

impl Claim {
    /// Stamps `last_finished_at`: the run is done and the next waits its interval.
    pub async fn finish(self, pool: &MySqlPool, now: NaiveDateTime) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE job_runs SET last_finished_at = ? WHERE job_key = ?")
            .bind(now)
            .bind(&self.key)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Gives the run back after a failure, so the next tick retries instead of
    /// waiting out the interval. Only if no later run has taken it since.
    pub async fn undo(self, pool: &MySqlPool) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE job_runs SET last_started_at = ? WHERE job_key = ? AND last_started_at = ?",
        )
        .bind(self.previous)
        .bind(&self.key)
        .bind(self.started_at)
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 1)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    async fn started(pool: &MySqlPool, key: &str) -> Option<NaiveDateTime> {
        sqlx::query_scalar("SELECT last_started_at FROM job_runs WHERE job_key = ?")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test_db::test]
    async fn a_run_is_claimed_once_per_interval(pool: MySqlPool) {
        let every = Duration::hours(6);
        let first = claim(&pool, "t", every, at(1)).await.unwrap();
        assert!(first.is_some());
        assert!(claim(&pool, "t", every, at(2)).await.unwrap().is_none());
        assert!(claim(&pool, "other", every, at(2)).await.unwrap().is_some());
        assert!(claim(&pool, "t", every, at(7)).await.unwrap().is_some());
    }

    #[test_db::test]
    async fn undo_restores_the_previous_start_only_while_it_is_ours(pool: MySqlPool) {
        let every = Duration::hours(6);
        let first = claim(&pool, "t", every, at(1)).await.unwrap().unwrap();
        first.undo(&pool).await.unwrap();
        assert_eq!(started(&pool, "t").await, None);
        let again = claim(&pool, "t", every, at(2)).await.unwrap().unwrap();
        let later = claim(&pool, "t", every, at(9)).await.unwrap().unwrap();
        again.undo(&pool).await.unwrap();
        assert_eq!(started(&pool, "t").await, Some(at(9)));
        later.undo(&pool).await.unwrap();
        assert_eq!(started(&pool, "t").await, Some(at(2)));
    }

    #[test_db::test]
    async fn overlapping_ticks_claim_exactly_once(pool: MySqlPool) {
        let every = Duration::hours(6);
        let claims =
            futures_util::future::join_all((0..5).map(|_| claim(&pool, "race", every, at(3))))
                .await;
        let won = claims
            .into_iter()
            .filter(|claim| claim.as_ref().unwrap().is_some())
            .count();
        assert_eq!(won, 1);
    }
}
