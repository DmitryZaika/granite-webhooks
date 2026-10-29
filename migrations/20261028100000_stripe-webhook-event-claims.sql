-- Stripe webhook event claims that survive a crash.
--
-- stripe_webhook_events.done_at: when the event's work finished. A claim without it
--                          (the Lambda timed out or crashed mid-work) is taken over by
--                          Stripe's retry once it is 5 minutes old, instead of the
--                          retry being answered as a duplicate and the event lost.
--                          Rows from before this change were finished claims:
--                          done_at = processed_at.
-- stripe_webhook_events INDEX(processed_at): the daily prune deletes by processed_at
--                          (a full scan before).
--
-- Each change is skipped when already done. Safe to run more than once.
-- ALGORITHM=INSTANT for the column, INPLACE with LOCK=NONE for the index: writes go
-- on meanwhile. lock_wait_timeout makes an ALTER fail instead of queueing queries
-- behind its metadata lock. A failed run leaves a success = 0 row in
-- _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'stripe_webhook_events'
      AND COLUMN_NAME = 'done_at') > 0,
  'DO 0',
  'ALTER TABLE stripe_webhook_events ADD COLUMN done_at DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

UPDATE stripe_webhook_events SET done_at = processed_at WHERE done_at IS NULL;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'stripe_webhook_events'
      AND INDEX_NAME = 'idx_stripe_webhook_events_processed_at') > 0,
  'DO 0',
  'ALTER TABLE stripe_webhook_events
     ADD INDEX idx_stripe_webhook_events_processed_at (processed_at),
     ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
