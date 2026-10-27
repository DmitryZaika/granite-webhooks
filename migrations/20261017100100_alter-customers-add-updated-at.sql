-- customers.updated_at, so the Granite Phone contact index can sync what
-- changed (and soft deletes) since the app's last sync. MySQL sets it on
-- every UPDATE of the row; existing rows stay NULL (a full download of the
-- index covers them). Guarded and INSTANT: customers is big and shared.
--
-- Then an index for those syncs and the index's version (a company's live
-- and deleted customers by when they changed), built online (INPLACE,
-- LOCK=NONE: reads and writes go on while it builds). A short lock wait
-- (3 s): when a long transaction holds the table, the migration fails and
-- is run again later instead of queueing every customers query behind it.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'customers'
      AND COLUMN_NAME = 'updated_at') = 0,
  'ALTER TABLE customers
     ADD COLUMN updated_at TIMESTAMP NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'customers'
      AND INDEX_NAME = 'idx_customers_company_changed') = 0,
  'ALTER TABLE customers
     ADD INDEX idx_customers_company_changed (company_id, deleted_at, updated_at),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
