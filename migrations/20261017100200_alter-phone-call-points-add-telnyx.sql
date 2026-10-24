-- Granite Phone calls (Telnyx) earn leaderboard points like CloudTalk and
-- RingCentral ones: phone_call_points.provider gets a 'telnyx' value.
-- Adding a value at the end of an ENUM is a metadata-only change
-- (INSTANT); guarded so a database that has it already is left alone. A
-- short lock wait (3 s): behind a long transaction the migration fails and
-- is run again, instead of stalling the table's other queries.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_call_points'
      AND COLUMN_NAME = 'provider' AND COLUMN_TYPE NOT LIKE '%''telnyx''%') = 1,
  'ALTER TABLE phone_call_points
     MODIFY COLUMN provider ENUM(''cloudtalk'', ''ringcentral'', ''telnyx'') NOT NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
