-- Cancellation requests and the amount-change notice.
--
-- billing_cancellation_requests: a Manager's "Request cancellation" on the
--   payments app's account page. 'scheduled' until the subscription ends at
--   effective_at ('completed') or the request is withdrawn ('withdrawn').
--   The onboarding-commitment job suspends the fee while one is scheduled.
-- company_billing.notified_monthly_cents: the monthly total (before tax) the
--   Managers were last told about; NULL until first seen (no email then).
-- company_billing.amount_notice_on: the day the last amount-change email
--   went out (at most one per day).
--
-- Each ADD COLUMN is skipped when the column exists and CREATE TABLE uses IF NOT
-- EXISTS. Safe to run more than once. ALGORITHM=INSTANT: metadata-only change.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS billing_cancellation_requests (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  requested_by INT NOT NULL,
  reason VARCHAR(1000) NULL,
  effective_at DATETIME NOT NULL,
  status ENUM('scheduled','withdrawn','completed') NOT NULL,
  requested_at DATETIME NOT NULL,
  withdrawn_at DATETIME NULL,
  withdrawn_by INT NULL,
  KEY idx_cancel_company (company_id, status),
  CONSTRAINT fk_cancel_company FOREIGN KEY (company_id) REFERENCES company (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'notified_monthly_cents') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN notified_monthly_cents INT NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'amount_notice_on') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN amount_notice_on DATE NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
