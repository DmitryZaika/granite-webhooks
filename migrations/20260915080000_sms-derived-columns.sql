-- SMS performance fix (2026-09-15).
--
-- The thread-list / unread-count / desktop-alert queries used to recompute
-- phone normalization, thread keys, company lines and echo exclusion over
-- the company's entire SMS history on every poll. These columns move that
-- work to insert time so the read path only touches indexed stored fields.
--
-- Mirrors scripts/sms-add-derived-columns.ts in the CRM repo.

CREATE TABLE IF NOT EXISTS sms_company_phones (
  provider VARCHAR(20) NOT NULL,
  company_id INT NOT NULL,
  phone10 VARCHAR(20) NOT NULL,
  agent_sender TINYINT(1) NOT NULL DEFAULT 0,
  inbound_recipient TINYINT(1) NOT NULL DEFAULT 0,
  PRIMARY KEY (provider, company_id, phone10)
);

-- cloudtalk_sms derived columns (MariaDB supports IF NOT EXISTS for columns)
ALTER TABLE cloudtalk_sms
  ADD COLUMN IF NOT EXISTS sender10 VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS recipient10 VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS phone_digits VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS is_echo TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS echo_of_sms_id INT NULL,
  ADD COLUMN IF NOT EXISTS agent_blank TINYINT(1) AS (agent IS NULL OR agent = '') STORED;

-- ringcentral_sms derived columns
ALTER TABLE ringcentral_sms
  ADD COLUMN IF NOT EXISTS sender10 VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS recipient10 VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS phone_digits VARCHAR(20) NULL,
  ADD COLUMN IF NOT EXISTS is_echo TINYINT(1) NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS echo_of_sms_id INT NULL,
  ADD COLUMN IF NOT EXISTS agent_blank TINYINT(1) AS (agent IS NULL OR agent = '') STORED;

-- Indexes the new read queries need.
ALTER TABLE cloudtalk_sms
  ADD KEY idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, recipient10, agent_blank),
  ADD KEY idx_sms_agent_threads (company_id, agent(64), phone_digits),
  ADD KEY idx_sms_inbound_recipient (company_id, direction, recipient10, phone_digits),
  ADD KEY idx_sms_echo_window (company_id, direction, status, created_date),
  ADD KEY idx_sms_echo_of (echo_of_sms_id),
  ADD KEY idx_sms_sender10 (company_id, sender10);

ALTER TABLE ringcentral_sms
  ADD KEY idx_sms_thread (company_id, is_echo, phone_digits, created_date, id, direction, status, sender10, recipient10, agent_blank),
  ADD KEY idx_sms_agent_threads (company_id, agent(64), phone_digits),
  ADD KEY idx_sms_inbound_recipient (company_id, direction, recipient10, phone_digits),
  ADD KEY idx_sms_echo_window (company_id, direction, status, created_date),
  ADD KEY idx_sms_echo_of (echo_of_sms_id),
  ADD KEY idx_sms_sender10 (company_id, sender10);
