-- A rep's sign-off for AI follow-up texts (`routes/followups/signature.rs`),
-- e.g. "Liza\nGranite Depot of Indianapolis": added under every text the AI
-- writes for them. Emails keep `email_signature`. NULL: none.
--
-- MySQL 8.0, guarded: the file is idempotent.

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'users'
      AND COLUMN_NAME = 'followup_sms_signature') = 0,
  'ALTER TABLE users ADD COLUMN followup_sms_signature VARCHAR(200) NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
