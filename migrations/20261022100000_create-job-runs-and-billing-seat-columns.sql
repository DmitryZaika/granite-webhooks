-- Job throttles and the per-user subscription columns.
--
-- job_runs: when each scheduled job (POST /v1/jobs/<job>) last started and
-- finished. Time-triggered calls the jobs every minute; a job that runs less
-- often (the 6-hourly billing reconcile, the daily webhook-event prune) checks
-- and stamps its row here, since Lambda containers do not keep memory.
--
-- company_billing.trial_end_at:       when the first charge is due (self-serve
--                                     signup), or NULL.
-- company_billing.extra_user_item_id: the subscription item billing users
--                                     beyond the included ones, or NULL.
-- company_billing.billable_users:     the billable user count last pushed to
--                                     Stripe, or NULL when never pushed.
--
-- Each ADD COLUMN is skipped when the column exists and CREATE TABLE uses IF NOT
-- EXISTS. Safe to run more than once. ALGORITHM=INSTANT: metadata-only change.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS job_runs (
  job_key VARCHAR(64) NOT NULL PRIMARY KEY,
  last_started_at DATETIME NULL,
  last_finished_at DATETIME NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'trial_end_at') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN trial_end_at DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'extra_user_item_id') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN extra_user_item_id VARCHAR(255) NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'billable_users') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN billable_users INT NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
