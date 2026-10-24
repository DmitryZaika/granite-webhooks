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

/// A pass of a batched job (companies by id, a batch per tick) this tick is
/// working on: a pass in progress, or a new one once the last finished
/// `every` ago (or never).
#[derive(Debug)]
pub struct BatchClaim {
    key: String,
    /// Work on ids after this one.
    pub after: i32,
}

/// Takes the next batch of `key`'s pass.
pub async fn claim_batch(
    pool: &MySqlPool,
    key: &str,
    every: Duration,
    now: NaiveDateTime,
) -> Result<Option<BatchClaim>, sqlx::Error> {
    sqlx::query("INSERT IGNORE INTO job_runs (job_key) VALUES (?)")
        .bind(key)
        .execute(pool)
        .await?;
    let mut tx = pool.begin().await?;
    let (finished, cursor): (Option<NaiveDateTime>, Option<i32>) = sqlx::query_as(
        "SELECT last_finished_at, cursor_id FROM job_runs WHERE job_key = ? FOR UPDATE",
    )
    .bind(key)
    .fetch_one(&mut *tx)
    .await?;
    let after = match cursor {
        Some(after) => after,
        None if finished.is_some_and(|at| at > now - every) => {
            tx.commit().await?;
            return Ok(None);
        }
        None => 0,
    };
    sqlx::query("UPDATE job_runs SET last_started_at = ?, cursor_id = ? WHERE job_key = ?")
        .bind(now)
        .bind(after)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(BatchClaim {
        key: key.to_string(),
        after,
    }))
}

impl BatchClaim {
    /// Records how far the batch got: the pass ends when `done`, else the
    /// next tick starts after `last_id` (or retries this batch without one).
    pub async fn advance(
        self,
        pool: &MySqlPool,
        last_id: Option<i32>,
        done: bool,
        now: NaiveDateTime,
    ) -> Result<(), sqlx::Error> {
        if done {
            sqlx::query(
                "UPDATE job_runs SET cursor_id = NULL, last_finished_at = ? WHERE job_key = ?",
            )
            .bind(now)
            .bind(&self.key)
            .execute(pool)
            .await?;
        } else if let Some(last_id) = last_id.filter(|id| *id > self.after) {
            sqlx::query("UPDATE job_runs SET cursor_id = ? WHERE job_key = ?")
                .bind(last_id)
                .bind(&self.key)
                .execute(pool)
                .await?;
        }
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
    async fn a_batched_pass_resumes_after_its_cursor_then_waits_its_interval(pool: MySqlPool) {
        let every = Duration::hours(6);
        let first = claim_batch(&pool, "b", every, at(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.after, 0);
        first.advance(&pool, Some(50), false, at(1)).await.unwrap();
        let second = claim_batch(&pool, "b", every, at(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.after, 50);
        // No progress (a failed listing): the same batch comes back.
        second.advance(&pool, None, false, at(1)).await.unwrap();
        let third = claim_batch(&pool, "b", every, at(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third.after, 50);
        third.advance(&pool, Some(80), true, at(2)).await.unwrap();
        assert!(
            claim_batch(&pool, "b", every, at(7))
                .await
                .unwrap()
                .is_none()
        );
        let next = claim_batch(&pool, "b", every, at(8))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.after, 0);
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
