-- Telnyx texting: texts from and to the company's Telnyx numbers, a third SMS
-- provider next to CloudTalk and RingCentral (`/v1/sms/telnyx/...`).
--
-- telnyx_company_config.messaging_profile_id: the company's Telnyx messaging
-- profile. Texting is on when the setup is enabled and this is set.
-- telnyx_sms (+ _thread_reads, _attachments): the shape of ringcentral_sms
-- with its derived thread columns (sender10, recipient10, phone_digits,
-- is_echo), plus Telnyx's own message id (a UUID), our line (number_id) and
-- the delivery outcome Telnyx reports (delivery_status, error_code,
-- delivered_at). `status` keeps the shared values: a delivered text is
-- 'sent' with delivery_status 'delivered'.
-- sms_opt_outs: phones that texted STOP to the company (or that Telnyx
-- refused as opted out, error 40300); automated texts skip them.
-- delayed_sms.provider and user_template_usage.template_type learn 'telnyx'.
--
-- MySQL 8.0. Every step is guarded (IF NOT EXISTS / information_schema), so
-- the file is idempotent. The ENUM changes only append a value (in place).

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'telnyx_company_config'
      AND COLUMN_NAME = 'messaging_profile_id') = 0,
  'ALTER TABLE telnyx_company_config ADD COLUMN messaging_profile_id VARCHAR(64) NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS telnyx_sms (
  id INT AUTO_INCREMENT PRIMARY KEY,
  -- Telnyx's message id (a UUID); NULL while our send is pending
  telnyx_id VARCHAR(64) NULL,
  -- E.164 digits without the plus; NULL only if Telnyx sent none
  sender BIGINT NULL,
  recipient BIGINT NOT NULL,
  text TEXT NOT NULL,
  direction ENUM('inbound','outbound') NOT NULL DEFAULT 'inbound',
  status ENUM('received','sent','failed','pending') NOT NULL DEFAULT 'received',
  error_message TEXT NULL,
  idempotency_key VARCHAR(64) NULL,
  -- outbound: the sending user's id (as text); inbound: NULL
  agent VARCHAR(255) NULL,
  sender_user_id INT NULL,
  created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  company_id INT NULL,
  sender10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  recipient10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  -- the thread key: the customer's phone (last ten digits)
  phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  is_echo TINYINT(1) NOT NULL DEFAULT 0,
  echo_of_sms_id INT NULL,
  is_automated TINYINT(1) NOT NULL DEFAULT 0,
  -- our line (telnyx_numbers.id) it went from or came to
  number_id INT NULL,
  -- Telnyx's to[].status: queued, sending, sent, delivered, sending_failed,
  -- delivery_failed, delivery_unconfirmed
  delivery_status VARCHAR(32) NULL,
  -- Telnyx's first errors[].code of a failed text (40300: opted out)
  error_code VARCHAR(16) NULL,
  delivered_at DATETIME NULL,
  UNIQUE KEY uniq_telnyx_id_per_company (company_id, telnyx_id),
  KEY idx_telnyx_sms_company_created (company_id, created_date),
  KEY idx_telnyx_sms_sender_user (sender_user_id),
  KEY idx_telnyx_sms_telnyx_id (telnyx_id),
  -- a retried flow step finds the text it already sent
  KEY idx_telnyx_sms_idempotency (company_id, idempotency_key),
  KEY idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent),
  KEY idx_sms_agent_threads (company_id, agent, phone_digits),
  KEY idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits),
  -- the threads our lines texted (a user's inbox)
  KEY idx_sms_outbound_sender (company_id, direction, sender10, phone_digits),
  KEY idx_sms_echo_window (company_id, direction, status, created_date),
  KEY idx_sms_sender10 (company_id, sender10)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

CREATE TABLE IF NOT EXISTS telnyx_sms_thread_reads (
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  customer_phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  last_read_at DATETIME NOT NULL,
  PRIMARY KEY (user_id, company_id, customer_phone_digits),
  KEY idx_company_user (company_id, user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

CREATE TABLE IF NOT EXISTS telnyx_sms_attachments (
  id INT AUTO_INCREMENT PRIMARY KEY,
  telnyx_sms_id INT NOT NULL,
  content_type VARCHAR(100) NOT NULL,
  filename VARCHAR(255) NOT NULL,
  s3_key VARCHAR(512) NOT NULL,
  s3_url VARCHAR(700) NOT NULL,
  width INT NULL,
  height INT NULL,
  position INT NOT NULL DEFAULT 0,
  created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT fk_telnyx_sms_attachments_sms
    FOREIGN KEY (telnyx_sms_id)
    REFERENCES telnyx_sms(id)
    ON DELETE CASCADE,
  -- an inbound MMS copied twice (Telnyx redelivers) keeps one row a file
  UNIQUE KEY uq_telnyx_sms_attachments_position (telnyx_sms_id, position)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS sms_opt_outs (
  company_id INT NOT NULL,
  -- the phone's last ten digits
  phone10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  -- 'stop': the customer texted STOP; 'carrier': Telnyx refused a text (40300)
  source VARCHAR(16) NOT NULL,
  opted_out_at DATETIME NOT NULL,
  PRIMARY KEY (company_id, phone10)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'delayed_sms'
      AND COLUMN_NAME = 'provider' AND COLUMN_TYPE NOT LIKE '%''telnyx''%') > 0,
  'ALTER TABLE delayed_sms
     MODIFY COLUMN provider ENUM(''cloudtalk'', ''ringcentral'', ''telnyx'') NOT NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'user_template_usage'
      AND COLUMN_NAME = 'template_type' AND COLUMN_TYPE NOT LIKE '%''telnyx_sms''%') > 0,
  'ALTER TABLE user_template_usage
     MODIFY COLUMN template_type ENUM(''cloudtalk_sms'', ''ringcentral_sms'', ''email'', ''telnyx_sms'') NOT NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
