-- SMS derived columns: sender10/recipient10/phone_digits/is_echo/echo_of_sms_id
-- hold the thread key and echo state so the read path only touches indexes.
--
-- MySQL 8.0 only. Every step is guarded (IF NOT EXISTS / information_schema),
-- so the file is idempotent and repairs a partially applied earlier run.
-- Column adds are INSTANT; each table's indexes go in one LOCK=NONE ALTER. The
-- phone columns carry an explicit collation so nothing joins across collations.
--
-- Rollout: deploy granite-webhooks -> run the CRM backfill
-- (scripts/sms-backfill-derived-fields.ts) -> deploy the CRM -> run it again.
--
-- Operator (RDS): run off-peak; lock_wait_timeout makes an ALTER fail instead
-- of queueing every query behind it. A failed run leaves a success = 0 row in
-- _sqlx_migrations that blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS sms_company_phones (
  provider ENUM('cloudtalk', 'ringcentral') NOT NULL,
  company_id INT NOT NULL,
  phone10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  -- the phone sent a message tagged with an agent
  agent_sender TINYINT(1) NOT NULL DEFAULT 0,
  -- the phone received an untagged inbound message
  inbound_recipient TINYINT(1) NOT NULL DEFAULT 0,
  PRIMARY KEY (provider, company_id, phone10)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

-- A table left by a partial earlier run has VARCHAR provider and the database
-- default collation; bring it to the definition above.
SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'sms_company_phones'
      AND ((COLUMN_NAME = 'provider' AND DATA_TYPE <> 'enum')
        OR (COLUMN_NAME = 'phone10' AND COLLATION_NAME <> 'utf8mb4_0900_ai_ci'))) > 0,
  'ALTER TABLE sms_company_phones
     MODIFY COLUMN provider ENUM(''cloudtalk'', ''ringcentral'') NOT NULL,
     MODIFY COLUMN phone10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
     DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

-- cloudtalk_sms -------------------------------------------------------------

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cloudtalk_sms' AND COLUMN_NAME = 'sender10') = 0,
  'ALTER TABLE cloudtalk_sms
     ADD COLUMN sender10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN recipient10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN is_echo TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN echo_of_sms_id INT NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

-- The read marker joins on phone_digits; give it the same collation.
SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cloudtalk_sms_thread_reads'
      AND COLUMN_NAME = 'customer_phone_digits' AND COLLATION_NAME <> 'utf8mb4_0900_ai_ci') > 0,
  'ALTER TABLE cloudtalk_sms_thread_reads
     MODIFY COLUMN customer_phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

-- idx_sms_thread            covers the thread rollups, so they never read the row
-- idx_sms_agent_threads     the agent's own threads ('mine', sales-rep filter)
-- idx_sms_inbound_recipient threads that wrote to the user's own line
-- idx_sms_echo_window       forward echo lookup: live outbound rows in the window
-- idx_sms_echo_of           un-hiding echoes when their outbound fails
-- idx_sms_sender10          re-keying a company line's historical inbound rows
SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cloudtalk_sms' AND INDEX_NAME = 'idx_sms_thread') = 0,
  'ALTER TABLE cloudtalk_sms
     ADD INDEX idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent),
     ADD INDEX idx_sms_agent_threads (company_id, agent, phone_digits),
     ADD INDEX idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits),
     ADD INDEX idx_sms_echo_window (company_id, direction, status, created_date),
     ADD INDEX idx_sms_echo_of (echo_of_sms_id),
     ADD INDEX idx_sms_sender10 (company_id, sender10),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

-- ringcentral_sms -----------------------------------------------------------

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ringcentral_sms' AND COLUMN_NAME = 'sender10') = 0,
  'ALTER TABLE ringcentral_sms
     ADD COLUMN sender10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN recipient10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
     ADD COLUMN is_echo TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN echo_of_sms_id INT NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ringcentral_sms_thread_reads'
      AND COLUMN_NAME = 'customer_phone_digits' AND COLLATION_NAME <> 'utf8mb4_0900_ai_ci') > 0,
  'ALTER TABLE ringcentral_sms_thread_reads
     MODIFY COLUMN customer_phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ringcentral_sms' AND INDEX_NAME = 'idx_sms_thread') = 0,
  'ALTER TABLE ringcentral_sms
     ADD INDEX idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent),
     ADD INDEX idx_sms_agent_threads (company_id, agent, phone_digits),
     ADD INDEX idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits),
     ADD INDEX idx_sms_echo_window (company_id, direction, status, created_date),
     ADD INDEX idx_sms_echo_of (echo_of_sms_id),
     ADD INDEX idx_sms_sender10 (company_id, sender10),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
