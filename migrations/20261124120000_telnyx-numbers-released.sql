-- Deleting a phone number on Admin > Phone releases it at Telnyx (billing
-- stops). The row stays, so the number's calls and texts keep it:
--
-- telnyx_numbers.released_at: when it was released; NULL = the company's
--   number. A released number is inactive, rings nobody and is not listed.
--
-- No AI Assistant for now: unanswered calls go to voicemail. The numbers set
-- to the AI Assistant move to it (bought, imported and ported numbers are
-- added with voicemail).
--
-- Guarded, so a database that has the column already is left alone. A short
-- lock wait (3 s): behind a long transaction the migration fails and is run
-- again, instead of stalling the table's other queries.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'telnyx_numbers'
      AND COLUMN_NAME = 'released_at') = 0,
  'ALTER TABLE telnyx_numbers
     ADD COLUMN released_at DATETIME NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql;
EXECUTE stmt;
DEALLOCATE PREPARE stmt;

UPDATE telnyx_numbers SET no_answer_action = 'voicemail' WHERE no_answer_action = 'ai';
