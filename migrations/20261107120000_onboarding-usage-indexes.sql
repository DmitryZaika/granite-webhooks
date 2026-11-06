-- Guided tours' "already used" checks (api/crates/accounts/src/routes/
-- onboarding/usage.rs) read these per user and per company:
-- - quote_revisions by who saved them (`quote_drawn`, `quote_cutout_added`):
--   the table had no index on created_by. (created_by, quote_id, created_at)
--   finds a user's revisions and their quotes from the index alone; only
--   the cutout check reads the drawing of those rows.
-- - ai_usage_log by company, feature and user (`chat_asked`, `ai_reply_used`,
--   `call_followed_up`): the log's indexes start with the user or the
--   company and the time, so each check read all of a user's (or a whole
--   company's) AI calls to find a few features.
--
-- Built online (INPLACE, LOCK=NONE: reads and writes go on while it builds)
-- and guarded: the file is idempotent. MySQL 8.0.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'quote_revisions'
      AND INDEX_NAME = 'idx_quote_revisions_created_by') = 0,
  'ALTER TABLE quote_revisions
     ADD INDEX idx_quote_revisions_created_by (created_by, quote_id, created_at),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ai_usage_log'
      AND INDEX_NAME = 'idx_ai_usage_log_company_feature_user') = 0,
  'ALTER TABLE ai_usage_log
     ADD INDEX idx_ai_usage_log_company_feature_user (company_id, feature, user_id, created_at),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
