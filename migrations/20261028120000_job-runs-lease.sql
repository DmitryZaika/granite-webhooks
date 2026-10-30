-- job_runs.leased_until: until when a batch of a batched job (the billing
--                        reconcile, the daily seat reconcile) belongs to the
--                        run that took it, or NULL. Overlapping ticks then
--                        skip it instead of repeating the same batch; the run
--                        clears it when it records its progress, and a run
--                        that died frees it by the time passing.
--
-- The ADD COLUMN is skipped when the column exists. Safe to run more than
-- once. ALGORITHM=INSTANT: metadata-only change. lock_wait_timeout makes the
-- ALTER fail instead of queueing queries behind its metadata lock. A failed
-- run leaves a success = 0 row in _sqlx_migrations that blocks sqlx: DELETE
-- it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'job_runs'
      AND COLUMN_NAME = 'leased_until') > 0,
  'DO 0',
  'ALTER TABLE job_runs ADD COLUMN leased_until DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
