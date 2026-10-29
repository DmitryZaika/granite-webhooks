-- The onboarding fee is gone (2026-10-28): its last columns follow.
--
-- signup_intents.fee_cents -> advance_cents: the advance payment the signup
--   consent states (one month at the plan chosen, before tax), frozen with
--   the consents.
-- company_onboarding_commitments.fee_cents, charge_after: dropped, nothing
--   reads them (signup_advance_cents and charge_at replaced them in
--   20261028110000).
--
-- These tables are new and hold no production rows. Each change is skipped
-- when already made. Safe to run more than once. ALGORITHM=INSTANT: a rename
-- and dropped columns are metadata-only on MySQL 8.0.29+. lock_wait_timeout
-- makes an ALTER fail instead of queueing queries behind its metadata lock. A
-- failed run leaves a success = 0 row in _sqlx_migrations that blocks sqlx:
-- DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'fee_cents') = 0,
  'DO 0',
  'ALTER TABLE signup_intents RENAME COLUMN fee_cents TO advance_cents, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitments'
      AND COLUMN_NAME = 'fee_cents') = 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitments DROP COLUMN fee_cents, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitments'
      AND COLUMN_NAME = 'charge_after') = 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitments DROP COLUMN charge_after, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
