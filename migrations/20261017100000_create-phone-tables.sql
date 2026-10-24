-- Granite Phone: business calling through Telnyx (docs/backend-design.md in
-- the granite-phone repo). Every table is new, so this file only adds.
--
-- device_sessions: the phone app's sign-in. Separate from the web `sessions`
-- on purpose: no 2-month cap (12 months, 30 idle days) and only the SHA-256
-- of the token is stored.
-- telnyx_company_config: the company's Telnyx resources and switches;
-- `webhook_token` names the company in Telnyx's callback URLs.
-- telnyx_numbers / telnyx_number_users: the company's numbers and who they
-- ring and who may call from them (same company, checked in code).
-- phone_devices: one per device session, holding its Telnyx telephony
-- credential and push token.
-- phone_calls (+ views, legs, events): the call log behind Recents. Events
-- are raw Telnyx callbacks kept 30 days; `dedupe_key` makes each one run
-- its side effects once.
-- phone_user_settings, phone_notice_acks, phone_audit_log: per-user phone
-- settings, accepted notices and the admin audit trail.
--
-- All dates are UTC (the pool uses time zone 'Z'). No triggers, views or
-- generated columns: test-db copies tables only.

CREATE TABLE IF NOT EXISTS device_sessions (
  id             BIGINT       NOT NULL AUTO_INCREMENT,
  user_id        INT          NOT NULL,
  company_id     INT          NOT NULL,
  -- SHA-256 hex of the token the app holds; the token is never stored
  token_hash     CHAR(64)     NOT NULL,
  device_name    VARCHAR(100) NOT NULL,
  platform       ENUM('android', 'ios') NOT NULL,
  app_version    VARCHAR(32)  NULL,
  created_at     DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  -- written at most every 5 minutes; idle 30 days = signed out
  last_seen_at   DATETIME     NULL,
  -- created_at + 12 months
  expires_at     DATETIME     NOT NULL,
  revoked_at     DATETIME     NULL,
  revoked_reason VARCHAR(32)  NULL,
  -- the last 911 alert this phone set off (alerts are deduplicated for 10 minutes)
  e911_alerted_at DATETIME    NULL,
  e911_alert_code VARCHAR(4)  NULL,
  -- "Report a problem" limit: 3 an hour (a window from its first report)
  diagnostics_window_start DATETIME NULL,
  diagnostics_window_count TINYINT UNSIGNED NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  UNIQUE KEY uq_device_sessions_token (token_hash),
  KEY idx_device_sessions_user (user_id, revoked_at),
  KEY idx_device_sessions_company (company_id),
  CONSTRAINT fk_device_sessions_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
  CONSTRAINT fk_device_sessions_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS telnyx_company_config (
  company_id                INT          NOT NULL,
  -- the company kill switch: off, no phone of the company can call
  enabled                   TINYINT(1)   NOT NULL DEFAULT 0,
  webhook_token             CHAR(32)     NOT NULL,
  credential_connection_id  VARCHAR(64)  NULL,
  texml_app_id              VARCHAR(64)  NULL,
  outbound_voice_profile_id VARCHAR(64)  NULL,
  billing_group_id          VARCHAR(64)  NULL,
  telnyx_account_id         VARCHAR(64)  NULL,
  ai_assistant_id           VARCHAR(128) NULL,
  fallback_texml_url        VARCHAR(512) NULL,
  recording_mode            ENUM('off', 'inbound', 'both') NOT NULL DEFAULT 'inbound',
  recording_notice_url      VARCHAR(512) NULL,
  business_hours            JSON         NULL,
  holidays                  JSON         NULL,
  min_app_version           VARCHAR(16)  NULL,
  daily_call_limit          INT          NOT NULL DEFAULT 300,
  -- the last 911 alert of a phone that proved no identity (deduplicated for
  -- 10 minutes per company)
  e911_alerted_at           DATETIME     NULL,
  e911_alert_code           VARCHAR(4)   NULL,
  created_at                DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at                DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                         ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id),
  UNIQUE KEY uq_telnyx_company_config_token (webhook_token),
  CONSTRAINT fk_telnyx_company_config_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS telnyx_numbers (
  id               INT          NOT NULL AUTO_INCREMENT,
  company_id       INT          NOT NULL,
  e164             VARCHAR(20)  NOT NULL,
  last10           CHAR(10)     NOT NULL,
  telnyx_number_id VARCHAR(64)  NULL,
  label            VARCHAR(100) NOT NULL DEFAULT '',
  kind             ENUM('direct', 'shared', 'main') NOT NULL DEFAULT 'direct',
  no_answer_action ENUM('ai', 'voicemail', 'forward') NOT NULL DEFAULT 'ai',
  forward_to       VARCHAR(20)  NULL,
  greeting         VARCHAR(500) NULL,
  active           TINYINT(1)   NOT NULL DEFAULT 1,
  created_at       DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at       DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_telnyx_numbers_e164 (e164),
  KEY idx_telnyx_numbers_company (company_id),
  KEY idx_telnyx_numbers_last10 (last10),
  CONSTRAINT fk_telnyx_numbers_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS telnyx_number_users (
  number_id     INT        NOT NULL,
  user_id       INT        NOT NULL,
  rings         TINYINT(1) NOT NULL DEFAULT 1,
  can_call_from TINYINT(1) NOT NULL DEFAULT 1,
  created_at    DATETIME   NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (number_id, user_id),
  KEY idx_telnyx_number_users_user (user_id),
  CONSTRAINT fk_telnyx_number_users_number FOREIGN KEY (number_id) REFERENCES telnyx_numbers (id) ON DELETE CASCADE,
  CONSTRAINT fk_telnyx_number_users_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_devices (
  id                    BIGINT       NOT NULL AUTO_INCREMENT,
  device_session_id     BIGINT       NOT NULL,
  user_id               INT          NOT NULL,
  company_id            INT          NOT NULL,
  platform              ENUM('android', 'ios') NOT NULL,
  device_name           VARCHAR(100) NOT NULL,
  app_version           VARCHAR(32)  NULL,
  telnyx_credential_id  VARCHAR(64)  NULL,
  sip_username          VARCHAR(128) NULL,
  credential_expires_at DATETIME     NULL,
  push_token            VARCHAR(512) NULL,
  push_token_updated_at DATETIME     NULL,
  -- login JWT limits: 1 a minute, 6 an hour
  last_token_at         DATETIME     NULL,
  token_window_start    DATETIME     NULL,
  token_window_count    INT          NOT NULL DEFAULT 0,
  revoked_at            DATETIME     NULL,
  revoke_reason         VARCHAR(32)  NULL,
  -- NULL while the Telnyx credential still has to be deleted (the job retries)
  credential_deleted_at DATETIME     NULL,
  -- failed credential deletes so far and the last try (the job backs off)
  credential_delete_attempts SMALLINT UNSIGNED NOT NULL DEFAULT 0,
  credential_delete_tried_at DATETIME NULL,
  created_at            DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at            DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                     ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_devices_session (device_session_id),
  UNIQUE KEY uq_phone_devices_sip_username (sip_username),
  KEY idx_phone_devices_user (user_id),
  KEY idx_phone_devices_company (company_id, revoked_at),
  KEY idx_phone_devices_push_token (push_token),
  CONSTRAINT fk_phone_devices_session FOREIGN KEY (device_session_id) REFERENCES device_sessions (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_calls (
  id                     BIGINT           NOT NULL AUTO_INCREMENT,
  company_id             INT              NOT NULL,
  -- who placed, answered or owns it; NULL while nobody did
  user_id                INT              NULL,
  direction              ENUM('inbound', 'outbound') NOT NULL,
  status                 ENUM('ringing', 'in_progress', 'answered', 'missed', 'declined',
                              'voicemail', 'ai_message', 'busy', 'no_answer', 'failed',
                              'canceled') NOT NULL DEFAULT 'ringing',
  other_number           VARCHAR(20)      NOT NULL,
  other_last10           CHAR(10)         NOT NULL,
  customer_id            INT              NULL,
  deal_id                BIGINT UNSIGNED  NULL,
  number_id              INT              NULL,
  our_number             VARCHAR(20)      NULL,
  caller_id              VARCHAR(20)      NULL,
  via_cell               TINYINT(1)       NOT NULL DEFAULT 0,
  telnyx_call_sid        VARCHAR(128)     NULL,
  telnyx_call_control_id VARCHAR(128)     NULL,
  telnyx_call_session_id VARCHAR(128)     NULL,
  started_at             DATETIME(3)      NOT NULL,
  answered_at            DATETIME(3)      NULL,
  ended_at               DATETIME(3)      NULL,
  duration_secs          INT              NULL,
  hangup_cause           VARCHAR(64)      NULL,
  answered_device_id     BIGINT           NULL,
  devices_rung           INT              NOT NULL DEFAULT 0,
  push_received_at       DATETIME(3)      NULL,
  recording_status       ENUM('none', 'pending', 'copied', 'failed') NOT NULL DEFAULT 'none',
  recording_telnyx_id    VARCHAR(128)     NULL,
  recording_key          VARCHAR(255)     NULL,
  recording_secs         INT              NULL,
  -- copies started so far (counted before each one, so a crash counts too)
  recording_attempts     TINYINT UNSIGNED NOT NULL DEFAULT 0,
  -- when Telnyx's copy of the recording was deleted (NULL: the job retries,
  -- backing off by the attempts and the last try)
  telnyx_deleted_at      DATETIME         NULL,
  telnyx_delete_attempts SMALLINT UNSIGNED NOT NULL DEFAULT 0,
  telnyx_delete_tried_at DATETIME         NULL,
  transcript_status      ENUM('none', 'pending', 'processing', 'done', 'failed', 'skipped')
                                          NOT NULL DEFAULT 'none',
  transcript             LONGTEXT         NULL,
  transcribe_attempts    TINYINT UNSIGNED NOT NULL DEFAULT 0,
  transcribe_claimed_at  DATETIME         NULL,
  ai_note                TEXT             NULL,
  note_id                BIGINT UNSIGNED  NULL,
  ai_notes               TINYINT(1)       NOT NULL DEFAULT 1,
  spam_likely            TINYINT(1)       NOT NULL DEFAULT 0,
  approved               TINYINT(1)       NOT NULL DEFAULT 0,
  created_at             DATETIME         NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at             DATETIME         NOT NULL DEFAULT CURRENT_TIMESTAMP
                                          ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_calls_call_sid (telnyx_call_sid),
  -- Recents pages (newest first) and recent calls of a company
  KEY idx_phone_calls_company (company_id, id),
  KEY idx_phone_calls_started (company_id, started_at),
  KEY idx_phone_calls_user (company_id, user_id, started_at),
  KEY idx_phone_calls_other (company_id, other_last10, started_at),
  KEY idx_phone_calls_customer (company_id, customer_id, started_at),
  KEY idx_phone_calls_call_control (telnyx_call_control_id),
  KEY idx_phone_calls_call_session (telnyx_call_session_id),
  KEY idx_phone_calls_transcript (transcript_status, transcribe_claimed_at),
  KEY idx_phone_calls_recording (recording_status, telnyx_deleted_at),
  -- the phone job: answered outbound calls for call points, calls left open
  KEY idx_phone_calls_ended (direction, status, ended_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_call_views (
  call_id   BIGINT   NOT NULL,
  user_id   INT      NOT NULL,
  seen_at   DATETIME NULL,
  hidden_at DATETIME NULL,
  PRIMARY KEY (call_id, user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_call_legs (
  id           BIGINT       NOT NULL AUTO_INCREMENT,
  call_id      BIGINT       NOT NULL,
  device_id    BIGINT       NULL,
  leg_call_sid VARCHAR(128) NULL,
  status       VARCHAR(32)  NOT NULL,
  created_at   DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at   DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                            ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_call_legs_sid (leg_call_sid),
  KEY idx_phone_call_legs_call (call_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_call_events (
  id          BIGINT      NOT NULL AUTO_INCREMENT,
  company_id  INT         NULL,
  call_id     BIGINT      NULL,
  source      ENUM('texml', 'call_control', 'app') NOT NULL,
  event_type  VARCHAR(64) NOT NULL,
  -- SHA-256 hex of Call Control `data.id`, or of the TeXML
  -- `CallSid|type|CallStatus|RecordingSid|DialCallStatus`
  dedupe_key  CHAR(64)    NOT NULL,
  occurred_at DATETIME(3) NULL,
  payload     MEDIUMTEXT  NULL,
  created_at  DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_call_events_dedupe (dedupe_key),
  KEY idx_phone_call_events_call (call_id),
  KEY idx_phone_call_events_created (created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_user_settings (
  user_id                  INT              NOT NULL,
  company_id               INT              NOT NULL,
  default_caller_number_id INT              NULL,
  ai_notes                 TINYINT(1)       NOT NULL DEFAULT 1,
  dnd_until                DATETIME         NULL,
  dnd_indefinite           TINYINT(1)       NOT NULL DEFAULT 0,
  working_hours            JSON             NULL,
  cell_number              VARCHAR(20)      NULL,
  cell_verified_at         DATETIME         NULL,
  cell_fallback            TINYINT(1)       NOT NULL DEFAULT 0,
  cell_pending_number      VARCHAR(20)      NULL,
  -- the 6-digit code our call reads aloud (the TeXML route needs it in
  -- clear); valid 10 minutes, cleared once confirmed
  cell_code                CHAR(6)          NULL,
  cell_code_expires_at     DATETIME         NULL,
  cell_code_attempts       TINYINT UNSIGNED NOT NULL DEFAULT 0,
  -- the last code call (never cleared: one a minute) and the day's count
  -- (5 a day, a window from its first call)
  cell_code_sent_at        DATETIME         NULL,
  cell_code_window_start   DATETIME         NULL,
  cell_code_window_count   TINYINT UNSIGNED NOT NULL DEFAULT 0,
  ring_timeout_secs        TINYINT UNSIGNED NOT NULL DEFAULT 25,
  updated_at               DATETIME         NOT NULL DEFAULT CURRENT_TIMESTAMP
                                            ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id),
  CONSTRAINT fk_phone_user_settings_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_notice_acks (
  id          BIGINT   NOT NULL AUTO_INCREMENT,
  user_id     INT      NOT NULL,
  device_id   BIGINT   NULL,
  notice      ENUM('recording', 'e911') NOT NULL,
  version     INT      NOT NULL,
  accepted_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_notice_acks (user_id, notice, version)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_audit_log (
  id            BIGINT       NOT NULL AUTO_INCREMENT,
  company_id    INT          NOT NULL,
  actor_user_id INT          NULL,
  action        VARCHAR(64)  NOT NULL,
  target        VARCHAR(128) NULL,
  details       JSON         NULL,
  created_at    DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
