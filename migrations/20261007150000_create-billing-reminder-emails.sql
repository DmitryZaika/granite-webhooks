-- Billing reminder emails (dunning), sent by the SPA's
-- api/subscription-reminders/process route on each time-triggered tick.
--
-- billing_reminder_emails: one row per recipient, per dunning cycle, per step.
-- The unique key is the send claim: the row is inserted (status 'sending')
-- BEFORE the email goes out, so overlapping ticks can never email twice. Rows
-- stuck in 'sending' are never resent.
--   cycle_kind:       payment_failed (a renewal failed) | first_payment (asked
--                     to pay, never subscribed)
--   cycle_started_at: company_billing.failed_payment_at, or
--                     company.billing_required_at, of the cycle
--   step:             started | three_days_left | pause_tomorrow | paused | resolved
--   status:           sending | sent | failed | unknown
--
-- company_billing.recovered_failed_at / recovered_at: the failed_payment_at of
-- the dunning cycle that last recovered, and when; written in the same
-- statement that sets payment_status back to active.
--
-- Each ADD COLUMN is skipped when the column exists and CREATE TABLE uses IF NOT
-- EXISTS. Safe to run more than once. ALGORITHM=INSTANT: metadata-only change.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'recovered_failed_at') > 0,
  'DO 0',
  'ALTER TABLE company_billing
     ADD COLUMN recovered_failed_at DATETIME NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'recovered_at') > 0,
  'DO 0',
  'ALTER TABLE company_billing
     ADD COLUMN recovered_at DATETIME NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS billing_reminder_emails (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    cycle_kind VARCHAR(16) NOT NULL,
    cycle_started_at DATETIME NOT NULL,
    step VARCHAR(32) NOT NULL,
    status VARCHAR(16) NOT NULL,
    attempts INT NOT NULL DEFAULT 1,
    error_code VARCHAR(64) NULL,
    ses_message_id VARCHAR(255) NULL,
    claimed_at DATETIME NOT NULL,
    sent_at DATETIME NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_billing_reminder (company_id, user_id, cycle_kind, cycle_started_at, step),
    KEY idx_billing_reminder_status (status, claimed_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
