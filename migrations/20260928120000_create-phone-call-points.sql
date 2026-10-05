-- Points for calls and SMS on the points leaderboard.
--
-- activity_points_sources: when each activity source starts to count. Nothing
-- before counts_from scores. Stored in UTC, like every date the app binds (the
-- pool uses timezone 'Z').
--
-- Rollout order: this migration, then the SPA (it writes is_automated, so an
-- SPA without this column fails every SMS send), then reset the start once the
-- SPA is live, because the old SPA cannot mark flow texts as automated:
--   UPDATE activity_points_sources SET counts_from = UTC_TIMESTAMP();
-- Then the time-triggered Lambda.
--
-- phone_call_points: one row per outbound CloudTalk / RingCentral call, pulled
-- from the providers' APIs by the SPA's /api/call-points/process on every
-- time-triggered tick. A call with more than 30 s of talk waits as 'pending'
-- until its recording is transcribed and classified. The SMS follow-up call
-- check may write a row first (user_id and started_at NULL); the sync fills
-- them in and never overwrites a classified outcome.
--
-- is_automated on the SMS tables marks messages the SMS flows sent for a rep,
-- which earn no points. Column adds are guarded and INSTANT, like
-- 20260915080000_sms-derived-columns.sql.

SET SESSION lock_wait_timeout = 30;

CREATE TABLE IF NOT EXISTS activity_points_sources (
  source      VARCHAR(32) NOT NULL,
  counts_from DATETIME    NOT NULL,
  PRIMARY KEY (source)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

INSERT IGNORE INTO activity_points_sources (source, counts_from)
VALUES ('calls', UTC_TIMESTAMP()), ('sms', UTC_TIMESTAMP());

CREATE TABLE IF NOT EXISTS phone_call_points (
  id                    BIGINT       NOT NULL AUTO_INCREMENT,
  company_id            INT          NOT NULL,
  provider              ENUM('cloudtalk', 'ringcentral') NOT NULL,
  provider_call_id      VARCHAR(64)  NOT NULL,
  -- NULL until the sync sees the call, or when no user is linked to the agent
  user_id               INT          NULL,
  customer_digits       VARCHAR(20)  NULL,
  -- UTC; NULL only on a row the call check wrote before the sync saw the call
  started_at            DATETIME     NULL,
  talk_seconds          INT          NOT NULL DEFAULT 0,
  outcome               ENUM('pending', 'attempt', 'voicemail', 'conversation')
                                     NOT NULL DEFAULT 'pending',
  recording_link        VARCHAR(1024) NULL,
  provider_voicemail    TINYINT(1)   NOT NULL DEFAULT 0,
  transcribe_attempts   TINYINT UNSIGNED NOT NULL DEFAULT 0,
  transcribe_claimed_at DATETIME     NULL,
  classified_at         DATETIME     NULL,
  created_at            DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at            DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                     ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_call_points_call (company_id, provider, provider_call_id),
  KEY idx_phone_call_points_scoring (company_id, started_at, user_id),
  KEY idx_phone_call_points_pending (outcome, transcribe_claimed_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'cloudtalk_sms'
      AND COLUMN_NAME = 'is_automated') = 0,
  'ALTER TABLE cloudtalk_sms
     ADD COLUMN is_automated TINYINT(1) NOT NULL DEFAULT 0,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'ringcentral_sms'
      AND COLUMN_NAME = 'is_automated') = 0,
  'ALTER TABLE ringcentral_sms
     ADD COLUMN is_automated TINYINT(1) NOT NULL DEFAULT 0,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
