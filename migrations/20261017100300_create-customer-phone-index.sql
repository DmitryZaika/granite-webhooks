-- Granite Phone: customers by phone number, so a call finds its customer
-- with an index lookup instead of reading every free-text phone of the
-- company (docs/backend-design.md in the granite-phone repo).
--
-- customer_phone_index: the last 10 digits of each live customer's phone
-- and phone 2. The phone job keeps it in step with the customers (what
-- changed by customers.updated_at, new customers by id), and the app's own
-- new contacts and added numbers go in at once.
-- customer_phone_index_builds: a company's first fill of the index, a
-- batch per job run; lookups use the index once it is done, and read the
-- customers as before until then.
-- phone_job_marks: how far a step of the phone job got (here: the customer
-- changes and new customers the index has taken).
--
-- customers gets an index on updated_at, so the job finds what changed
-- since its last run without reading the table: built online (INPLACE,
-- LOCK=NONE: reads and writes go on while it builds) and guarded. A short
-- lock wait (3 s): when a long transaction holds the table, the migration
-- fails and is run again later instead of queueing every customers query
-- behind it.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'customers'
      AND INDEX_NAME = 'idx_customers_updated') = 0,
  'ALTER TABLE customers
     ADD INDEX idx_customers_updated (updated_at),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS customer_phone_index (
  company_id  INT      NOT NULL,
  -- the last 10 digits of the customer's phone or phone 2
  last10      CHAR(10) NOT NULL,
  customer_id INT      NOT NULL,
  PRIMARY KEY (company_id, last10, customer_id),
  KEY idx_customer_phone_index_customer (customer_id),
  CONSTRAINT fk_customer_phone_index_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS customer_phone_index_builds (
  company_id INT      NOT NULL,
  -- the first fill's progress: the company's customers up to this id are in
  done_id    INT      NOT NULL DEFAULT 0,
  -- when the first fill finished; lookups use the index from then on
  built_at   DATETIME NULL,
  PRIMARY KEY (company_id),
  CONSTRAINT fk_customer_phone_index_builds_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_job_marks (
  name       VARCHAR(64) NOT NULL,
  -- the step has taken everything up to this time ...
  value      DATETIME    NULL,
  -- ... and, of that time (or by id alone), up to this id
  last_id    BIGINT      NULL,
  updated_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
