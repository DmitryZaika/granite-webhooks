-- Email inbox list: index customers_emails by email.
--
-- The inbox list (general_datebase employee.emails / admin.emails) resolves each
-- row's sender and receiver name with
--   SELECT c.name FROM customers c JOIN customers_emails ce ON ce.customer_id = c.id
--   WHERE ce.email = e.sender_email ... ORDER BY ce.id LIMIT 1
-- Without an index on email that is a full scan of customers_emails for every
-- listed message, so one inbox page examined 2 to 3 million rows in production.
--
-- general_datebase/scripts/add-hot-path-indexes.ts may already have created the
-- same index (same name, same columns) by hand. So the ADD is skipped when an
-- index of that name exists, or when any index on customers_emails already
-- starts with `email` (it serves the same lookup). Safe to run more than once.
--
-- Online DDL: reads and writes keep working while the index builds.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'customers_emails'
      AND (INDEX_NAME = 'idx_customers_emails_email'
        OR (SEQ_IN_INDEX = 1 AND COLUMN_NAME = 'email'))) > 0,
  'DO 0',
  'ALTER TABLE customers_emails
     ADD INDEX idx_customers_emails_email (email, customer_id),
     ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
