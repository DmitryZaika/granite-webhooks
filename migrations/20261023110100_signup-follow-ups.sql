-- Self-serve signup follow-ups.
--
-- company.max_users:            nullable; NULL = no user limit. Self-serve
--                               companies pay per user and get NULL; existing
--                               companies keep their value.
-- signup_intents.gave_up_at:    when the maintenance job stopped resuming a
--                               stuck provisioning (and alerted staff), or NULL.
-- consent_records unique key:   (signup_intent_id, kind, document_version), so
--                               the terms checkbox keeps one row per document
--                               it accepts (terms, privacy, dpa).
--
-- Each change is skipped when already done. Safe to run more than once.
-- lock_wait_timeout makes an ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT IS_NULLABLE FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'max_users') = 'YES',
  'DO 0',
  'ALTER TABLE company MODIFY COLUMN max_users INT NULL DEFAULT 10');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND COLUMN_NAME = 'gave_up_at') > 0,
  'DO 0',
  'ALTER TABLE signup_intents ADD COLUMN gave_up_at DATETIME NULL, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'consent_records'
      AND INDEX_NAME = 'uq_consent_signup_kind_document') > 0,
  'DO 0',
  'ALTER TABLE consent_records ADD UNIQUE KEY uq_consent_signup_kind_document (signup_intent_id, kind, document_version)');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'consent_records'
      AND INDEX_NAME = 'uq_consent_signup_kind') > 0,
  'ALTER TABLE consent_records DROP INDEX uq_consent_signup_kind',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
