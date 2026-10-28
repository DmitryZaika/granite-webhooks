-- Card and ACH bank payment methods (self-serve billing step 13).
--
-- signup_intents.bank_verification_until:  set when a signup was completed with a
--                          bank account still waiting for micro-deposits: the owner
--                          uses the CRM until then, and the subscription is made once
--                          Stripe verifies the account. Stays set afterwards.
-- company_billing.pending_setup_intent_id: a bank account a Manager added that waits
--                          for micro-deposits; it becomes the default once verified.
-- company_billing.method_hold, method_hold_at, method_hold_until: the saved method
--                          stopped working (blocked bank account, or a dispute);
--                          access pauses at method_hold_until unless a new method is
--                          saved.
-- company_billing.payment_processing_at:  a bank payment is on its way (ACH takes up
--                          to 4 business days); the gate does not pause meanwhile.
-- plan_invites.setup_intent_id: the invite's payment method setup, so a bank account
--                          verified days later can still accept the plan.
--
-- Each change is skipped when already done. Safe to run more than once.
-- ALGORITHM=INSTANT: metadata-only changes. lock_wait_timeout makes an ALTER fail
-- instead of queueing queries behind its metadata lock. A failed run leaves a
-- success = 0 row in _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'bank_verification_until') > 0,
  'DO 0',
  'ALTER TABLE signup_intents ADD COLUMN bank_verification_until DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'pending_setup_intent_id') > 0,
  'DO 0',
  'ALTER TABLE company_billing
     ADD COLUMN pending_setup_intent_id VARCHAR(255) NULL,
     ADD COLUMN method_hold ENUM(''blocked'',''disputed'') NULL,
     ADD COLUMN method_hold_at DATETIME NULL,
     ADD COLUMN method_hold_until DATETIME NULL,
     ADD COLUMN payment_processing_at DATETIME NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'plan_invites'
      AND COLUMN_NAME = 'setup_intent_id') > 0,
  'DO 0',
  'ALTER TABLE plan_invites ADD COLUMN setup_intent_id VARCHAR(255) NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
