-- SMS derived columns (2026-09-15 performance fix).
--
-- The thread-list / unread-count / desktop-alert queries used to recompute
-- phone normalization, thread keys, company lines and echo exclusion over the
-- company's entire SMS history on every poll. These columns move that work to
-- insert time so the read path only touches indexed stored fields.
--
-- MySQL 8.0 only. Every step is guarded by information_schema, so the file is
-- idempotent: it can be re-run after a partial failure, and it repairs the
-- state an earlier (MariaDB-syntax) version of this migration left behind
-- (its CREATE TABLE succeeded, its ALTER failed). Plain column adds are
-- INSTANT (no table rebuild); each table's indexes are built by ONE in-place
-- ALTER with LOCK=NONE (one metadata-lock upgrade per table, not six). Every
-- new phone column carries an explicit collation so joins between the SMS
-- tables, the thread-read markers and sms_company_phones never mix
-- collations, whatever the database default is.
--
-- `agent` is normalized at write time (TRIM, '' -> NULL) and by the backfill,
-- so "untagged" is simply `agent IS NULL` and no generated column is needed.
--
-- Operator checklist (RDS):
--   1. SELECT version, success FROM _sqlx_migrations WHERE version = 20260915080000;
--      a row with success = 0 blocks sqlx: DELETE it, then re-run.
--   2. Run off-peak. lock_wait_timeout below makes an ALTER fail instead of
--      queueing every query behind it; on failure fix step 1 and re-run.
--   3. Deploy granite-webhooks, run the CRM backfill, deploy the CRM, run the
--      backfill once more (see scripts/sms-backfill-derived-fields.ts).

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

-- A table left by the earlier migration attempt has VARCHAR provider and the
-- database default collation; bring it to this definition (it is empty or
-- tiny, so the rebuild is instant).
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

-- idx_sms_thread            covering index for the thread rollups: everything
--                           the list / unread / alert aggregates read, so they
--                           never touch the row (and never load `text`)
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
