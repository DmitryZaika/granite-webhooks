-- Native eSignatures (replaces SignWell). Schema the app expects, from
-- general_datebase tests/nativeSignatureDatabase.ts and
-- docs/signatures/schema-handoff.md.

-- Both ALTERs are skipped where they were already applied by hand (some dev
-- databases), since MySQL has no ADD COLUMN / ADD KEY IF NOT EXISTS.

-- Per-company switch. NULL keeps the legacy rule (on when a SignWell key is
-- set); 1 or 0 is explicit and needs no SignWell key.
SET @sql = IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'signature_enabled') = 0,
  'ALTER TABLE company ADD COLUMN signature_enabled TINYINT(1) NULL',
  'DO 0'
);
PREPARE stmt FROM @sql;
EXECUTE stmt;
DEALLOCATE PREPARE stmt;

-- The native tables point at (request, company) so a row can never belong to
-- another company's request.
SET @sql = IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'signature_requests'
      AND INDEX_NAME = 'uq_native_owner') = 0,
  'ALTER TABLE signature_requests ADD UNIQUE KEY uq_native_owner (id, company_id)',
  'DO 0'
);
PREPARE stmt FROM @sql;
EXECUTE stmt;
DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS native_signature_documents (
  request_id INT PRIMARY KEY,
  company_id INT NOT NULL,
  document_id CHAR(36) NOT NULL UNIQUE,
  prepared_object_key VARCHAR(1024) NOT NULL,
  prepared_sha256 CHAR(64) NOT NULL,
  document_version CHAR(64) NOT NULL,
  token_hash CHAR(64) NOT NULL UNIQUE,
  fields_json JSON NOT NULL,
  settings_json JSON NOT NULL,
  completed_object_key VARCHAR(1024) NULL,
  completed_sha256 CHAR(64) NULL,
  expires_at DATETIME(3) NULL,
  revoked_at DATETIME(3) NULL,
  sent_at DATETIME(3) NULL,
  viewed_at DATETIME(3) NULL,
  signed_at DATETIME(3) NULL,
  CONSTRAINT fk_native_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id),
  INDEX idx_native_expiration (expires_at, request_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_sessions (
  session_hash CHAR(64) PRIMARY KEY,
  request_id INT NOT NULL,
  company_id INT NOT NULL,
  invitation_token_hash CHAR(64) NOT NULL,
  expires_at DATETIME(3) NOT NULL,
  reviewed_at DATETIME(3) NULL,
  created_at DATETIME(3) NOT NULL,
  CONSTRAINT fk_session_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id),
  INDEX idx_session_expiry (expires_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_finalizations (
  request_id INT PRIMARY KEY,
  company_id INT NOT NULL,
  idempotency_key VARCHAR(64) NOT NULL,
  submission_sha256 CHAR(64) NOT NULL,
  submission_json JSON NOT NULL,
  audit_json JSON NOT NULL,
  signed_at DATETIME(3) NOT NULL,
  state ENUM('rendering','retryable','completed') NOT NULL,
  lease_owner CHAR(36) NULL,
  lease_until DATETIME(3) NULL,
  completed_object_key VARCHAR(1024) NULL,
  completed_sha256 CHAR(64) NULL,
  evidence_object_key VARCHAR(1024) NULL,
  CONSTRAINT fk_finalization_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_audit (
  id BIGINT PRIMARY KEY AUTO_INCREMENT,
  request_id INT NOT NULL,
  company_id INT NOT NULL,
  event_key VARCHAR(100) NOT NULL,
  event_type VARCHAR(32) NOT NULL,
  evidence_json JSON NOT NULL,
  created_at DATETIME(3) NOT NULL,
  UNIQUE KEY uq_native_event (request_id, event_key),
  CONSTRAINT fk_audit_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_deliveries (
  id BIGINT PRIMARY KEY AUTO_INCREMENT,
  company_id INT NOT NULL,
  created_by_user_id INT NOT NULL,
  logical_send_hash CHAR(64) NOT NULL,
  payload_sha256 CHAR(64) NOT NULL,
  state ENUM('prepared','sending','uncertain','accepted','rejected','recorded') NOT NULL,
  lease_owner CHAR(36) NULL,
  lease_until DATETIME(3) NULL,
  encrypted_mail LONGTEXT NULL,
  accepted_at DATETIME(3) NULL,
  email_id INT NULL,
  created_at DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  UNIQUE KEY uq_native_delivery (company_id, created_by_user_id, logical_send_hash),
  UNIQUE KEY uq_native_delivery_owner (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_delivery_items (
  delivery_id BIGINT NOT NULL,
  company_id INT NOT NULL,
  request_id INT NOT NULL,
  attachment_index INT NOT NULL,
  PRIMARY KEY (delivery_id, attachment_index),
  UNIQUE KEY uq_native_delivery_request (request_id),
  CONSTRAINT fk_delivery_owner FOREIGN KEY (delivery_id, company_id)
    REFERENCES native_signature_deliveries (id, company_id),
  CONSTRAINT fk_delivery_request_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS native_signature_jobs (
  id BIGINT PRIMARY KEY AUTO_INCREMENT,
  request_id INT NOT NULL,
  company_id INT NOT NULL,
  job_key VARCHAR(32) NOT NULL,
  kind ENUM('reminder','notification') NOT NULL,
  due_at DATETIME(3) NOT NULL,
  state ENUM('pending','sending','retry','sent','failed','uncertain','blocked','cancelled') NOT NULL,
  attempts INT NOT NULL DEFAULT 0,
  lease_owner CHAR(36) NULL,
  lease_until DATETIME(3) NULL,
  sent_at DATETIME(3) NULL,
  message_id VARCHAR(255) NULL,
  UNIQUE KEY uq_native_job (request_id, company_id, job_key),
  INDEX idx_native_job_due (state, due_at),
  CONSTRAINT fk_native_job_owner FOREIGN KEY (request_id, company_id)
    REFERENCES signature_requests (id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Granite Depot of Indianapolis signs natively from the start.
UPDATE company SET signature_enabled = 1 WHERE id = 1;
