-- Points for emails on the points leaderboard: 1 point per email a rep sends.
--
-- activity_points_sources row 'emails': nothing sent before counts_from scores.
--
-- is_automated on emails marks messages the app sent for a rep (SMS-flow
-- follow-up emails, scheduled template emails recorded into history), which
-- earn no points, like is_automated on the SMS tables
-- (20260928120000_create-phone-call-points.sql). Guarded and INSTANT.
--
-- Rollout order: this migration, then the SPA (it writes is_automated, so an
-- SPA without this column fails those inserts), then reset the start once the
-- SPA is live, because the old SPA cannot mark automated emails:
--   UPDATE activity_points_sources SET counts_from = UTC_TIMESTAMP() WHERE source = 'emails';

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'emails'
      AND COLUMN_NAME = 'is_automated') = 0,
  'ALTER TABLE emails
     ADD COLUMN is_automated TINYINT(1) NOT NULL DEFAULT 0,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

INSERT IGNORE INTO activity_points_sources (source, counts_from)
VALUES ('emails', UTC_TIMESTAMP());
