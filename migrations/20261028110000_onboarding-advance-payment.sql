-- Onboarding with Dima is free: the onboarding fee becomes the advance
-- payment (2026-10-28). A commitment not completed by its deadline adds one
-- month of the subscription, paid in advance, to the next subscription
-- invoice at least 7 days after its notice; once that invoice is paid the
-- same amount is credited to the customer's balance, so the following
-- invoice is covered.
--
-- company_onboarding_commitments:
--   status:               + advance_scheduled (the pending invoice item is in
--                         Stripe), advance_paid (its invoice paid, the credit
--                         granted), advance_skipped (the deadline or the
--                         charge window passed while
--                         ONBOARDING_ADVANCE_CHARGING was off, or without
--                         consent). The fee_* values stay for old rows and
--                         are never written: rows holding them are moved to
--                         void (fee_invoiced, fee_paid, fee_failed) or
--                         advance_skipped (fee_skipped).
--   fee_cents:            unused (default 0 from now on).
--   charge_after:         unused.
--   signup_advance_cents: the advance the signup consent stated (one month at
--                         the plan chosen: base, add-ons, extra users);
--                         backfilled as fee_cents / 2 for older rows.
--   advance_cents:        the advance, fixed when the notice cycle starts: one
--                         month of the live subscription's items (unit amount
--                         x quantity, before tax).
--   charge_at:            the subscription invoice the advance goes on.
--   prepaid_from_at, prepaid_until_at: the month the advance pays for (the
--                         period after charge_at).
--   advance_item_id:      the invoice item (`ii_...`), pending until Stripe
--                         sweeps it into the invoice at charge_at; cleared
--                         when deleted or its invoice is void.
--   stripe_invoice_id:    now the subscription invoice that carried the item.
--   advance_credit_id:    the customer balance transaction (`cbtxn_...`) of
--                         the credit.
--   review_hold_at, review_hold_by: staff hold the advance for review (Terms
--                         11.7): no item while held.
-- company_onboarding_commitment_events.from_status, to_status: 32 characters
--   (advance_scheduled has 17).
-- billing_cancellation_requests:
--   requested_by:         NULL for a request found in Stripe (channel stripe).
--   channel:              + stripe: Stripe had the subscription set to end
--                         with no request recorded here (a request whose save
--                         was lost); the webhook sync records it.
--   signup_intent_id:     a request made while the signup's bank account
--                         still waited for micro-deposits (no subscription
--                         yet): the signup never makes one.
--
-- Each change is skipped when already made, and the UPDATEs only touch rows
-- still in the old state. Safe to run more than once. ALGORITHM=INSTANT where
-- MySQL allows it (columns added at the end, ENUM values appended); widening
-- the event VARCHARs and making requested_by nullable are INPLACE, LOCK=NONE.
-- lock_wait_timeout makes an ALTER fail instead of queueing queries behind
-- its metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations
-- that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitments'
      AND COLUMN_NAME = 'status'
      AND COLUMN_TYPE LIKE '%advance_scheduled%') > 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitments MODIFY COLUMN status ENUM(''pending'',''booked'',''completed'',''waived'',''suspended'',''notice_sent'',''fee_invoiced'',''fee_paid'',''fee_failed'',''fee_skipped'',''void'',''advance_scheduled'',''advance_paid'',''advance_skipped'') NOT NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitments'
      AND COLUMN_NAME = 'fee_cents'
      AND COLUMN_DEFAULT IS NOT NULL) > 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitments ALTER COLUMN fee_cents SET DEFAULT 0, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitments'
      AND COLUMN_NAME = 'signup_advance_cents') > 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitments
     ADD COLUMN signup_advance_cents INT NOT NULL DEFAULT 0,
     ADD COLUMN advance_cents INT NULL,
     ADD COLUMN charge_at DATETIME NULL,
     ADD COLUMN prepaid_from_at DATETIME NULL,
     ADD COLUMN prepaid_until_at DATETIME NULL,
     ADD COLUMN advance_item_id VARCHAR(255) NULL,
     ADD COLUMN advance_credit_id VARCHAR(255) NULL,
     ADD COLUMN review_hold_at DATETIME NULL,
     ADD COLUMN review_hold_by INT NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

UPDATE company_onboarding_commitments
   SET signup_advance_cents = fee_cents DIV 2
 WHERE signup_advance_cents = 0 AND fee_cents > 0;

UPDATE company_onboarding_commitments
   SET status = 'void', version = version + 1, updated_at = UTC_TIMESTAMP()
 WHERE status IN ('fee_invoiced', 'fee_paid', 'fee_failed');

UPDATE company_onboarding_commitments
   SET status = 'advance_skipped', version = version + 1, updated_at = UTC_TIMESTAMP()
 WHERE status = 'fee_skipped';

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_onboarding_commitment_events'
      AND COLUMN_NAME = 'to_status'
      AND CHARACTER_MAXIMUM_LENGTH >= 32) > 0,
  'DO 0',
  'ALTER TABLE company_onboarding_commitment_events
     MODIFY COLUMN from_status VARCHAR(32) NULL,
     MODIFY COLUMN to_status VARCHAR(32) NOT NULL,
     ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'billing_cancellation_requests'
      AND COLUMN_NAME = 'requested_by'
      AND IS_NULLABLE = 'YES') > 0,
  'DO 0',
  'ALTER TABLE billing_cancellation_requests MODIFY COLUMN requested_by INT NULL, ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'billing_cancellation_requests'
      AND COLUMN_NAME = 'channel'
      AND COLUMN_TYPE LIKE '%stripe%') > 0,
  'DO 0',
  'ALTER TABLE billing_cancellation_requests MODIFY COLUMN channel ENUM(''app'',''email'',''stripe'') NOT NULL DEFAULT ''app'', ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'billing_cancellation_requests'
      AND COLUMN_NAME = 'signup_intent_id') > 0,
  'DO 0',
  'ALTER TABLE billing_cancellation_requests ADD COLUMN signup_intent_id CHAR(36) NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
