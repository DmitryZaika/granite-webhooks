-- RingCentral schema (CloudTalk parity) plus the OAuth connect columns.
--
-- Companies connect through the shared Granite Manager app (QuickBooks pattern):
-- encrypted per-company tokens live in ringcentral_access_token /
-- ringcentral_refresh_token. The ringcentral_client_id / _client_secret / _jwt
-- columns stay as the pre-Connect fallback that app/utils/ringcentral.server.ts
-- still reads when a company has no OAuth connection.
ALTER TABLE company
  ADD COLUMN ringcentral_client_id VARCHAR(255) NULL,
  ADD COLUMN ringcentral_client_secret VARCHAR(255) NULL,
  ADD COLUMN ringcentral_jwt TEXT NULL,
  ADD COLUMN ringcentral_server_url VARCHAR(255) NULL,
  ADD COLUMN ringcentral_access_token BLOB NULL,
  ADD COLUMN ringcentral_refresh_token BLOB NULL,
  ADD COLUMN ringcentral_access_expires BIGINT NULL,
  ADD COLUMN ringcentral_refresh_expires BIGINT NULL,
  ADD COLUMN ringcentral_account_id VARCHAR(64) NULL,
  ADD COLUMN ringcentral_connected_at DATETIME NULL,
  ADD COLUMN ringcentral_connected_by INT NULL;

ALTER TABLE users
  ADD COLUMN ringcentral_extension_id VARCHAR(64) NULL,
  ADD COLUMN ringcentral_phone_number VARCHAR(20) NULL;

CREATE TABLE IF NOT EXISTS ringcentral_sms (
  id INT AUTO_INCREMENT PRIMARY KEY,
  ringcentral_id BIGINT NULL,
  sender BIGINT NULL,
  recipient BIGINT NOT NULL,
  text TEXT NOT NULL,
  direction ENUM('inbound','outbound') NOT NULL DEFAULT 'inbound',
  status ENUM('received','sent','failed','pending') NOT NULL DEFAULT 'received',
  error_message TEXT NULL,
  idempotency_key VARCHAR(36) NULL,
  agent VARCHAR(255) NULL,
  sender_user_id INT NULL,
  created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  company_id INT NULL,
  KEY idx_ringcentral_sms_company_phones (company_id, sender, recipient),
  KEY idx_ringcentral_sms_company_created (company_id, created_date),
  KEY idx_ringcentral_sms_sender_user (sender_user_id),
  UNIQUE KEY uniq_ringcentral_id_per_company (company_id, ringcentral_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS ringcentral_sms_thread_reads (
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  customer_phone_digits VARCHAR(20) NOT NULL,
  last_read_at DATETIME NOT NULL,
  PRIMARY KEY (user_id, company_id, customer_phone_digits)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS ringcentral_sms_templates (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NULL,
  name VARCHAR(255) NOT NULL,
  body TEXT NOT NULL,
  position INT NOT NULL DEFAULT 0,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  KEY idx_ringcentral_sms_templates_user_list (company_id, user_id, deleted_at, position)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS ringcentral_contacts (
  id INT AUTO_INCREMENT PRIMARY KEY,
  customer_id INT NOT NULL,
  company_id INT NOT NULL,
  ringcentral_id BIGINT NOT NULL,
  phone_e164_1 VARCHAR(20) NULL,
  phone_e164_2 VARCHAR(20) NULL,
  last_synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  last_error TEXT NULL,
  UNIQUE KEY uniq_ringcentral_customer (customer_id),
  KEY idx_company_ringcentral (company_id, ringcentral_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS ringcentral_sms_attachments (
  id INT AUTO_INCREMENT PRIMARY KEY,
  ringcentral_sms_id INT NOT NULL,
  content_type VARCHAR(100) NOT NULL,
  filename VARCHAR(255) NOT NULL,
  s3_key VARCHAR(512) NOT NULL,
  s3_url VARCHAR(700) NOT NULL,
  width INT NULL,
  height INT NULL,
  position INT NOT NULL DEFAULT 0,
  created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT fk_rc_sms_attachments_sms
    FOREIGN KEY (ringcentral_sms_id)
    REFERENCES ringcentral_sms(id)
    ON DELETE CASCADE,
  KEY idx_rc_sms_attachments_sms (ringcentral_sms_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
