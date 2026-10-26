-- Move to plan: a superuser invites a legacy company's Managers to accept a
-- subscription plan with chosen add-ons and a first charge date.
--
-- plan_invites:       one row per invite. status: open -> accepting (a Manager
--                     accepted the consents; the Stripe subscription is being
--                     made) -> accepted, or revoked (by a superuser, or replaced
--                     by a newer invite). An open invite past expires_at is
--                     expired (shown so, never accepted).
--                     first_charge_on: the day shown; first_charge_at: its
--                     instant (UTC). legacy_subscription_id: set when a live
--                     legacy Stripe subscription forced the date to its period
--                     end (it is cancelled at that end on acceptance).
--                     trial_end_at, price_ids, stripe_customer_id,
--                     payment_method_id: frozen at acceptance, so a resumed run
--                     sends Stripe the same request under the same idempotency
--                     key. live_company_id (generated) + its unique key: at most
--                     one open or accepting invite per company.
-- plan_invite_tokens: the links emailed to the Managers, one per Manager per
--                     send. Only the SHA-256 (hex) of the token is stored.
-- consent_records:    unique (plan_invite_id, kind, document_version): one set
--                     of consents per invite.
--
-- CREATE TABLE uses IF NOT EXISTS and the index is skipped when it exists. Safe
-- to run more than once. lock_wait_timeout makes the ALTER fail instead of
-- queueing queries behind its metadata lock. A failed run leaves a success = 0
-- row in _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS plan_invites (
  id CHAR(36) NOT NULL PRIMARY KEY,
  company_id INT NOT NULL,
  created_by INT NOT NULL,
  addons JSON NOT NULL,
  first_charge_on DATE NOT NULL,
  first_charge_at DATETIME NOT NULL,
  legacy_subscription_id VARCHAR(255) NULL,
  expires_at DATETIME NOT NULL,
  status ENUM('open','accepting','accepted','revoked') NOT NULL DEFAULT 'open',
  accepted_by INT NULL,
  accepting_at DATETIME NULL,
  trial_end_at DATETIME NULL,
  price_ids JSON NULL,
  stripe_customer_id VARCHAR(255) NULL,
  payment_method_id VARCHAR(255) NULL,
  stripe_subscription_id VARCHAR(255) NULL,
  provision_attempts TINYINT UNSIGNED NOT NULL DEFAULT 0,
  last_error VARCHAR(255) NULL,
  gave_up_at DATETIME NULL,
  accepted_at DATETIME NULL,
  revoked_at DATETIME NULL,
  revoked_by INT NULL,
  created_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  live_company_id INT AS (IF(status IN ('open','accepting'), company_id, NULL)) STORED,
  UNIQUE KEY uq_plan_invites_live_company (live_company_id),
  KEY idx_plan_invites_company (company_id, created_at),
  KEY idx_plan_invites_status (status, updated_at),
  CONSTRAINT fk_plan_invites_company FOREIGN KEY (company_id) REFERENCES company (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS plan_invite_tokens (
  token_sha256 CHAR(64) NOT NULL PRIMARY KEY,
  invite_id CHAR(36) NOT NULL,
  user_id INT NOT NULL,
  created_at DATETIME NOT NULL,
  KEY idx_plan_invite_tokens_invite (invite_id),
  CONSTRAINT fk_plan_invite_tokens_invite FOREIGN KEY (invite_id) REFERENCES plan_invites (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'consent_records'
      AND INDEX_NAME = 'uq_consent_invite_kind_document') > 0,
  'DO 0',
  'ALTER TABLE consent_records ADD UNIQUE KEY uq_consent_invite_kind_document (plan_invite_id, kind, document_version)');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
