-- Self-serve signup: the signup in progress and the consents accepted at it.
--
-- signup_intents:  one row per signup started on payments.granite-manager.com.
--                  The id (UUIDv4) is also the bearer secret of the public flow.
--                  status: started -> verified (email code) -> payment_ready
--                  (SetupIntent) -> provisioning -> completed, or expired.
--                  awaiting_bank_verification is reserved for ACH micro-deposits
--                  (step 13): such rows expire after 11 days, others after 24 hours.
--                  code_hash: HMAC-SHA256 of the 6-digit email code, never the code.
--                  trial_end_at, fee_cents, deadline_at, price_ids: frozen when the
--                  consents are accepted, so a resumed provisioning sends Stripe the
--                  same request under the same idempotency key.
-- consent_records: append-only evidence, one row per checkbox per acceptance
--                  (04-legal.md section 7). signup_intent_id is set first;
--                  company_id and user_id once the company exists.
-- company_billing.seat_change_seq: bumped on every billable-user change; the
--                  Stripe idempotency key of the seat push (seats:<company>:<seq>).
--
-- CREATE TABLE uses IF NOT EXISTS and the ADD COLUMN is skipped when the column
-- exists. Safe to run more than once. ALGORITHM=INSTANT: metadata-only change.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS signup_intents (
  id CHAR(36) NOT NULL PRIMARY KEY,
  status ENUM('started','verified','payment_ready','awaiting_bank_verification',
              'provisioning','completed','expired') NOT NULL DEFAULT 'started',
  email VARCHAR(255) NOT NULL,
  code_hash CHAR(64) NULL,
  code_sent_at DATETIME NULL,
  code_expires_at DATETIME NULL,
  code_attempts TINYINT UNSIGNED NOT NULL DEFAULT 0,
  codes_sent TINYINT UNSIGNED NOT NULL DEFAULT 0,
  verified_at DATETIME NULL,
  company JSON NOT NULL,
  owner_name VARCHAR(100) NOT NULL,
  password_hash VARCHAR(100) NOT NULL,
  plan JSON NOT NULL,
  stripe_customer_id VARCHAR(255) NULL,
  setup_intent_id VARCHAR(255) NULL,
  setup_attempts TINYINT UNSIGNED NOT NULL DEFAULT 0,
  payment_method_id VARCHAR(255) NULL,
  accepted_at DATETIME NULL,
  trial_end_at DATETIME NULL,
  fee_cents INT NULL,
  deadline_at DATETIME NULL,
  price_ids JSON NULL,
  stripe_subscription_id VARCHAR(255) NULL,
  provision_attempts TINYINT UNSIGNED NOT NULL DEFAULT 0,
  last_error VARCHAR(255) NULL,
  company_id INT NULL,
  user_id INT NULL,
  ip VARCHAR(45) NULL,
  user_agent VARCHAR(512) NULL,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  expires_at DATETIME NOT NULL,
  completed_at DATETIME NULL,
  KEY idx_signup_intents_status (status, updated_at),
  KEY idx_signup_intents_email (email),
  UNIQUE KEY uq_signup_intents_company (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS consent_records (
  id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NULL,
  user_id INT NULL,
  signup_intent_id CHAR(36) NULL,
  plan_invite_id CHAR(36) NULL,
  source ENUM('self_serve','staff_move') NOT NULL,
  kind VARCHAR(32) NOT NULL,
  document_version VARCHAR(64) NOT NULL,
  document_sha256 CHAR(64) NULL,
  checkbox_text TEXT NOT NULL,
  checkbox_sha256 CHAR(64) NOT NULL,
  amounts_and_dates JSON NOT NULL,
  stripe_customer_id VARCHAR(255) NULL,
  payment_method_id VARCHAR(255) NULL,
  stripe_mandate_id VARCHAR(255) NULL,
  sec_code_intended ENUM('WEB','CCD') NULL,
  ip VARCHAR(45) NULL,
  user_agent VARCHAR(512) NULL,
  locale VARCHAR(16) NULL,
  accepted_at DATETIME NOT NULL,
  KEY idx_consent_company (company_id, kind),
  UNIQUE KEY uq_consent_signup_kind (signup_intent_id, kind)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company_billing'
      AND COLUMN_NAME = 'seat_change_seq') > 0,
  'DO 0',
  'ALTER TABLE company_billing ADD COLUMN seat_change_seq INT NOT NULL DEFAULT 0, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
