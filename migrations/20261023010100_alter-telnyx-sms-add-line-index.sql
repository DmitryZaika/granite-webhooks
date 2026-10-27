-- The phone app's Texts tab threads a user's texts by line and other number
-- (`phone/texts.rs`): index (company, line, other number, id) so a user's
-- threads, their unread counts and a thread's texts read the index only.
--
-- MySQL 8.0, guarded: the file is idempotent.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'telnyx_sms'
      AND INDEX_NAME = 'idx_telnyx_sms_line_thread') = 0,
  'ALTER TABLE telnyx_sms
     ADD INDEX idx_telnyx_sms_line_thread (company_id, number_id, phone_digits, id),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
