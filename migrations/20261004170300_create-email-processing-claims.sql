-- A row means one Lambda invocation is processing this inbound email right now.
-- EventBridge retries an email after its 5 s timeout while the first run may
-- still be working; the retry sees the row and answers 429, so EventBridge tries
-- again later instead of both runs doing the same slow work. The row is deleted
-- when the run ends; a run killed by the 30 s Lambda timeout leaves it behind,
-- and the next claim takes it over once it is older than 40 s.
--
-- Only granite-webhooks reads or writes this table. The Lambda keeps working
-- without it (no claim, today's behaviour), so the migration and the deploy can
-- go in either order.
--
-- message_id copies the length, character set and collation of
-- emails.message_id, read from information_schema, so both columns compare a
-- Message-ID the same way. Fails loudly if emails.message_id is missing.

SET @message_id_type := (
  SELECT CONCAT(
    'VARCHAR(', CHARACTER_MAXIMUM_LENGTH, ') CHARACTER SET ', CHARACTER_SET_NAME,
    ' COLLATE ', COLLATION_NAME)
  FROM information_schema.COLUMNS
  WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'emails' AND COLUMN_NAME = 'message_id'
);

SET @sql := CONCAT(
  'CREATE TABLE IF NOT EXISTS email_processing_claims (
     message_id ', @message_id_type, ' NOT NULL PRIMARY KEY,
     claim_token CHAR(36) NOT NULL,
     claimed_at DATETIME(3) NOT NULL
   ) ENGINE=InnoDB');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
