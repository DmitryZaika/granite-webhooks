-- The disputed payment behind a payment method hold (self-serve billing 13b).
--
-- company_billing.method_hold_charge_id:    the disputed charge of a 'disputed' hold,
--                          so charge.dispute.closed (won) clears that hold only.
-- company_billing.method_hold_amount_cents: the disputed amount, for the Managers' email.
-- company_billing.method_hold_charged_at:   when the disputed payment was made, for the
--                          same email.
-- All three are NULL for a 'blocked' hold and cleared with the hold.
--
-- Skipped when already done. Safe to run more than once. ALGORITHM=INSTANT:
-- metadata-only change. lock_wait_timeout makes the ALTER fail instead of queueing
-- queries behind its metadata lock. A failed run leaves a success = 0 row in
-- _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'method_hold_charge_id') > 0,
  'DO 0',
  'ALTER TABLE company_billing
     ADD COLUMN method_hold_charge_id VARCHAR(255) NULL,
     ADD COLUMN method_hold_amount_cents BIGINT NULL,
     ADD COLUMN method_hold_charged_at DATETIME NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
