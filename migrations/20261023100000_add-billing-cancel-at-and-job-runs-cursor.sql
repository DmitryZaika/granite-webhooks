-- Billing reminder upkeep.
--
-- company_billing.cancel_at: when the Stripe subscription is scheduled to end
--                            (cancel_at, or the period end with
--                            cancel_at_period_end), or NULL. Synced from the
--                            subscription webhooks; a trial cancelled before
--                            its end gets no first-charge email.
-- job_runs.cursor_id:        the last company id a batched job (the billing
--                            reconcile) reached in its current pass, or NULL
--                            between passes.
--
-- Each ADD COLUMN is skipped when the column exists. Safe to run more than
-- once. ALGORITHM=INSTANT: metadata-only change. lock_wait_timeout makes the
-- ALTER fail instead of queueing queries behind its metadata lock. A failed
-- run leaves a success = 0 row in _sqlx_migrations that blocks sqlx: DELETE
-- it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'cancel_at') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN cancel_at DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'job_runs'
      AND COLUMN_NAME = 'cursor_id') > 0,
  'DO 0',
  'ALTER TABLE job_runs ADD COLUMN cursor_id INT NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
