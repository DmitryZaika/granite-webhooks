-- SMS derived columns (2026-09-15 performance fix).
--
-- The thread-list / unread-count / desktop-alert queries used to recompute
-- phone normalization, thread keys, company lines and echo exclusion over the
-- company's entire SMS history on every poll. These columns move that work to
-- insert time so the read path only touches indexed stored fields.
--
-- MySQL 8.0 only: no `IF NOT EXISTS` on ALTER TABLE, plain column adds are
-- INSTANT (no table rebuild), indexes are built in place with LOCK=NONE.
-- Every new phone column carries an explicit collation so joins between the
-- SMS tables, the thread-read markers and sms_company_phones never mix
-- collations, whatever the database default is.
--
-- `agent` is normalized at write time (TRIM, '' -> NULL) and by the backfill,
-- so "untagged" is simply `agent IS NULL` and no generated column is needed.
--
-- Rollout order (each step is safe against the previous one):
--   1. this migration, off-peak
--   2. deploy granite-webhooks (writes the derived columns)
--   3. general_datebase: bun scripts/sms-backfill-derived-fields.ts
--   4. deploy general_datebase (reads the derived columns)
--   5. run the backfill once more for rows the old SPA inserted in the gap

CREATE TABLE sms_company_phones (
  provider ENUM('cloudtalk', 'ringcentral') NOT NULL,
  company_id INT NOT NULL,
  phone10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  -- the phone sent a message tagged with an agent
  agent_sender TINYINT(1) NOT NULL DEFAULT 0,
  -- the phone received an untagged inbound message
  inbound_recipient TINYINT(1) NOT NULL DEFAULT 0,
  PRIMARY KEY (provider, company_id, phone10)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci;

-- cloudtalk_sms -------------------------------------------------------------

ALTER TABLE cloudtalk_sms
  ADD COLUMN sender10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN recipient10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN is_echo TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN echo_of_sms_id INT NULL,
  ALGORITHM=INSTANT;

ALTER TABLE cloudtalk_sms_thread_reads
  MODIFY COLUMN customer_phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL;

-- Covering index for the thread rollups: everything the list / unread /
-- alert aggregates read, so they never touch the row (and never load `text`).
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent),
  ALGORITHM=INPLACE, LOCK=NONE;
-- The agent's own threads (scope 'mine' agent branch, sales-rep filter).
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_agent_threads (company_id, agent, phone_digits),
  ALGORITHM=INPLACE, LOCK=NONE;
-- Threads that wrote to the user's own line (scope 'mine' phone branch).
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits),
  ALGORITHM=INPLACE, LOCK=NONE;
-- Forward echo lookup: live outbound rows inside the echo window.
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_echo_window (company_id, direction, status, created_date),
  ALGORITHM=INPLACE, LOCK=NONE;
-- Un-hiding echoes when their outbound fails.
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_echo_of (echo_of_sms_id),
  ALGORITHM=INPLACE, LOCK=NONE;
-- Re-keying a company line's historical inbound rows.
ALTER TABLE cloudtalk_sms
  ADD INDEX idx_sms_sender10 (company_id, sender10),
  ALGORITHM=INPLACE, LOCK=NONE;

-- ringcentral_sms -----------------------------------------------------------

ALTER TABLE ringcentral_sms
  ADD COLUMN sender10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN recipient10 VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL,
  ADD COLUMN is_echo TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN echo_of_sms_id INT NULL,
  ALGORITHM=INSTANT;

ALTER TABLE ringcentral_sms_thread_reads
  MODIFY COLUMN customer_phone_digits VARCHAR(20) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL;

ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, agent),
  ALGORITHM=INPLACE, LOCK=NONE;
ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_agent_threads (company_id, agent, phone_digits),
  ALGORITHM=INPLACE, LOCK=NONE;
ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_inbound_recipient (company_id, direction, recipient10, agent, phone_digits),
  ALGORITHM=INPLACE, LOCK=NONE;
ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_echo_window (company_id, direction, status, created_date),
  ALGORITHM=INPLACE, LOCK=NONE;
ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_echo_of (echo_of_sms_id),
  ALGORITHM=INPLACE, LOCK=NONE;
ALTER TABLE ringcentral_sms
  ADD INDEX idx_sms_sender10 (company_id, sender10),
  ALGORITHM=INPLACE, LOCK=NONE;
