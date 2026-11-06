-- Forward-only numbers: a company number (often one ported in from an old
-- carrier) that rings nobody and passes everything on.
--
-- telnyx_numbers.forward_only: every call to the number goes straight to
--   forward_to (no phones ring, no voicemail, no AI Assistant).
-- telnyx_numbers.forward_texts_to: E.164; every text to the number is sent
--   on from it to this number, with the sender's number in front. NULL =
--   texts stay in the CRM only.
--
-- Guarded, so a database that has the columns already is left alone. A
-- short lock wait (3 s): behind a long transaction the migration fails and
-- is run again, instead of stalling the table's other queries.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'telnyx_numbers'
      AND COLUMN_NAME = 'forward_only') = 0,
  'ALTER TABLE telnyx_numbers
     ADD COLUMN forward_only TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN forward_texts_to VARCHAR(20) NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql;
EXECUTE stmt;
DEALLOCATE PREPARE stmt;
