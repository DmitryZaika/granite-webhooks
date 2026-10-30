-- Disputes one by one, a bank payment that failed per invoice, and who lifted a
-- payment method hold (self-serve billing 13b).
--
-- billing_disputes: every Stripe dispute of a company's charge (charge.dispute.*).
--   status:              Stripe's dispute status as last read (warning_needs_response,
--                        warning_under_review, warning_closed, needs_response,
--                        under_review, won, lost, prevented).
--   closed_at:           when it closed (won, lost, warning_closed); NULL while open.
--   resolved_by_user_id, resolved_at: a superuser lifted the company's hold while this
--                        dispute was open or lost; it never holds the company again.
--   An open or lost dispute not resolved by staff keeps a 'disputed' hold on the
--   company (a new payment method does not lift it).
-- billing_bank_payment_failures: an invoice whose bank (ACH) payment failed
--   (charge.failed). A later bank payment of that invoice no longer lifts a billing
--   pause while it is on its way: access returns when the money arrives.
-- company_billing.method_hold_cleared_by, method_hold_cleared_at: the superuser who
--   last lifted the company's payment method hold, and when.
--
-- Each change is skipped when already done. Safe to run more than once.
-- ALGORITHM=INSTANT: metadata-only change. lock_wait_timeout makes the ALTER fail
-- instead of queueing queries behind its metadata lock. A failed run leaves a
-- success = 0 row in _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS billing_disputes (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    stripe_dispute_id VARCHAR(255) NOT NULL,
    charge_id VARCHAR(255) NOT NULL,
    amount_cents BIGINT NULL,
    status VARCHAR(32) NOT NULL,
    created_at DATETIME NOT NULL,
    closed_at DATETIME NULL,
    resolved_by_user_id INT NULL,
    resolved_at DATETIME NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_billing_disputes_dispute (stripe_dispute_id),
    KEY idx_billing_disputes_company (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS billing_bank_payment_failures (
    stripe_invoice_id VARCHAR(255) NOT NULL PRIMARY KEY,
    company_id INT NOT NULL,
    failed_at DATETIME NOT NULL,
    KEY idx_billing_bank_payment_failures_company (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'method_hold_cleared_by') > 0,
  'DO 0',
  'ALTER TABLE company_billing
     ADD COLUMN method_hold_cleared_by INT NULL,
     ADD COLUMN method_hold_cleared_at DATETIME NULL,
     ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
