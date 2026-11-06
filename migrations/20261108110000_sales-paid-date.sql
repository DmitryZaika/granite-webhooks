-- Projects' Paid & Completed comes from the transaction's paid date
-- (`sales.paid_date`, set when the Transactions page marks it paid). Added
-- only where it is missing: some databases have it already.
SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'sales' AND COLUMN_NAME = 'paid_date') = 0,
  'ALTER TABLE sales ADD COLUMN paid_date DATETIME NULL',
  'DO 0'
);
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
