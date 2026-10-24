-- When the rep was told that a follow-up ended without them: the customer
-- replied, or the run failed (`followups/alerts.rs`). It is the claim, so a
-- run is told once. Runs that ended before this migration count as told
-- (their updated_at kept).
--
-- MySQL 8.0, guarded: the file is idempotent.

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'sms_flow_enrollments'
      AND COLUMN_NAME = 'ended_alert_at') = 0,
  'ALTER TABLE sms_flow_enrollments ADD COLUMN ended_alert_at DATETIME NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

UPDATE sms_flow_enrollments
   SET ended_alert_at = COALESCE(updated_at, started_at), updated_at = updated_at
 WHERE status IN ('stopped_by_reply', 'failed', 'completed', 'cancelled')
   AND ended_alert_at IS NULL;
