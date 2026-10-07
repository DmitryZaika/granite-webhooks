-- Hot-path indexes that only ever existed as one-off scripts
-- (general_datebase/scripts/add-hot-path-indexes.ts and
-- add-emails-company-folder-index.ts).
--
-- idx_notifications_user_open: /api/notifications is polled every minute by every
-- open tab and filtered on (user_id, is_done); without it the query scans all
-- notifications.
-- idx_emails_company_folder: unread, trash and badge counts filter emails by
-- company and folder. It was added to the production database by hand on
-- 2026-09-28, so the ADD is skipped where it already exists.
--
-- Each ADD is skipped when an index of that name exists. Safe to run more than
-- once. Online DDL: reads and writes keep working while the index builds.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'notifications'
      AND INDEX_NAME = 'idx_notifications_user_open') > 0,
  'DO 0',
  'ALTER TABLE notifications
     ADD INDEX idx_notifications_user_open (user_id, is_done),
     ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'emails'
      AND INDEX_NAME = 'idx_emails_company_folder') > 0,
  'DO 0',
  'ALTER TABLE emails
     ADD INDEX idx_emails_company_folder (company_id, is_draft, deleted_at, sent_at),
     ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
