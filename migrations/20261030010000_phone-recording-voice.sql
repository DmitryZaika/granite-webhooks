-- phone_user_settings.recording_voice: the user's calls say "This call is
-- being recorded." / "This call may be recorded." to both sides when their
-- recording starts (on by default). Off (My Account > Phone lines > Granite
-- Manager): the user's outgoing calls, Call via my cell calls and the Record
-- button record without it. Incoming calls keep their notice (it plays
-- before anyone answers).
--
-- Guarded, so a database that has it already is left alone; a short lock
-- wait (3 s), as the other phone migrations.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_user_settings'
      AND COLUMN_NAME = 'recording_voice') = 0,
  'ALTER TABLE phone_user_settings
     ADD COLUMN recording_voice TINYINT(1) NOT NULL DEFAULT 1,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
