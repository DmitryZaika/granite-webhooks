-- When Stripe payment was last required for a company.
--
-- A company that is asked to pay and has not subscribed gets 7 days from this
-- moment, then access pauses until it subscribes (general_datebase
-- stripeBilling.ts FIRST_PAYMENT_DAYS). The superuser toggle sets it when it
-- turns payment on.
--
-- Companies that already require payment get the time of this migration, so
-- their 7 days start at deploy rather than locking them out at once.
--
-- Safe to run more than once: the column is added only when it is missing, and
-- the backfill touches only rows without a date.

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'billing_required_at') > 0,
  'DO 0',
  'ALTER TABLE company ADD COLUMN billing_required_at DATETIME NULL AFTER billing_required');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

UPDATE company
   SET billing_required_at = UTC_TIMESTAMP()
 WHERE billing_required = 1
   AND billing_required_at IS NULL;
