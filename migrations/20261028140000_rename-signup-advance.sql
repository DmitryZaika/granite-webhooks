-- The onboarding fee is gone (2026-10-28): its last columns follow.
--
-- signup_intents.fee_cents -> advance_cents: the advance payment the signup
--   consent states (one month at the plan chosen, before tax: base, add-ons
--   and the extra users of the estimate), frozen with the consents. A row
--   accepted under the fee holds 2 x (base + add-ons); its advance is the
--   monthly_cents its consent records stated (base, add-ons, extra users),
--   else half the fee (base + add-ons, without extra users). advance_cents is
--   added, filled from fee_cents while that column exists, then fee_cents is
--   dropped, so a run that stopped halfway fills it again from the same fee.
-- company_onboarding_commitments.fee_cents, charge_after: dropped, nothing
--   reads them (signup_advance_cents and charge_at replaced them in
--   20261028110000).
--
-- These tables are new and hold no production rows. Each change is skipped
-- when already made. Safe to run more than once. ALGORITHM=INSTANT: added
-- and dropped columns are metadata-only on MySQL 8.0.29+. lock_wait_timeout
-- makes an ALTER fail instead of queueing queries behind its metadata lock. A
-- failed run leaves a success = 0 row in _sqlx_migrations that blocks sqlx:
-- DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'advance_cents') > 0,
  'DO 0',
  'ALTER TABLE signup_intents ADD COLUMN advance_cents INT NULL AFTER trial_end_at, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'fee_cents') = 0,
  'DO 0',
  'UPDATE signup_intents si
      SET si.advance_cents = COALESCE(
            (SELECT CAST(JSON_UNQUOTE(JSON_EXTRACT(cr.amounts_and_dates, ''$.monthly_cents'')) AS SIGNED)
               FROM consent_records cr
              WHERE cr.signup_intent_id = si.id
                AND JSON_EXTRACT(cr.amounts_and_dates, ''$.monthly_cents'') IS NOT NULL
              ORDER BY cr.id
              LIMIT 1),
            si.fee_cents DIV 2)
    WHERE si.fee_cents IS NOT NULL');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'fee_cents') = 0,
  'DO 0',
  'ALTER TABLE signup_intents DROP COLUMN fee_cents, ALGORITHM=INSTANT');
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
