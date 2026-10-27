-- Granite Phone in the CRM: the browser is a phone too. The CRM's phone dock
-- registers the browser with the web session cookie as a phone device of
-- platform 'web', with its own Telnyx credential, so it rings with the
-- user's app phones and places calls the same way.
--
-- device_sessions.platform / phone_devices.platform: + 'web' (at the end of
-- the ENUM: metadata only, INSTANT).
-- device_sessions.web_session_id: the web session (`sessions.id`) a browser
-- phone belongs to. Signing that session out (or its end) signs the browser
-- phone out; NULL for the app's sign-ins.
-- phone_devices.online_until: a browser phone rings only while its tab says
-- it is connected to Telnyx (it renews this every minute; NULL = offline).
-- App phones ignore it (Telnyx wakes them with its own push).
-- phone_calls.live_recording / live_recording_sid: the Record button of a
-- live call. 'none' = never pressed (the company's recording mode decides),
-- 'recording' / 'paused' = what the user chose last; the sid is the TeXML
-- recording we started or paused (TeXML calls only).
--
-- Every change is guarded, so a database that has it already is left alone.
-- A short lock wait (3 s): behind a long transaction the migration fails and
-- is run again, instead of stalling the tables' other queries.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'device_sessions'
      AND COLUMN_NAME = 'platform' AND COLUMN_TYPE NOT LIKE '%''web''%') = 1,
  'ALTER TABLE device_sessions
     MODIFY COLUMN platform ENUM(''android'', ''ios'', ''web'') NOT NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_devices'
      AND COLUMN_NAME = 'platform' AND COLUMN_TYPE NOT LIKE '%''web''%') = 1,
  'ALTER TABLE phone_devices
     MODIFY COLUMN platform ENUM(''android'', ''ios'', ''web'') NOT NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'device_sessions'
      AND COLUMN_NAME = 'web_session_id') = 0,
  'ALTER TABLE device_sessions ADD COLUMN web_session_id CHAR(36) NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'device_sessions'
      AND INDEX_NAME = 'idx_device_sessions_web_session') = 0,
  'ALTER TABLE device_sessions
     ADD INDEX idx_device_sessions_web_session (web_session_id, revoked_at),
     ALGORITHM=INPLACE, LOCK=NONE',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_devices'
      AND COLUMN_NAME = 'online_until') = 0,
  'ALTER TABLE phone_devices ADD COLUMN online_until DATETIME NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_calls'
      AND COLUMN_NAME = 'live_recording') = 0,
  'ALTER TABLE phone_calls
     ADD COLUMN live_recording ENUM(''none'', ''recording'', ''paused'') NOT NULL DEFAULT ''none'',
     ADD COLUMN live_recording_sid VARCHAR(128) NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
