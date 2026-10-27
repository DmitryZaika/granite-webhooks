-- Granite Phone call settings, as RingCentral has them in its phone settings:
-- hold music, incoming call rules, incoming call information, and the
-- company's spam and blocked numbers.
--
-- phone_user_settings (per user):
--   forward_all / forward_all_to / forward_all_number: "Forward all calls",
--     to voicemail or to a US or Canada number (E.164).
--   display_number: what the user's phones show for an incoming call, the
--     caller's number (`caller`) or the company number they called
--     (`called`); unknown_show_mine: show the called number when the caller
--     hides theirs.
--   announce_direct / announce_shared / announce_line_name: an announcement
--     played to the user before a call connects (calls to their direct
--     lines, to shared and main lines; with the line's name).
--   require_pin / answer_pin_hash: a fallback cell asks for this PIN instead
--     of "press 1" (SHA-256 hex of `granite-phone-pin:{user id}:{pin}`; the
--     PIN itself is never stored).
--   hold_music / hold_music_url / hold_music_name: what callers hear while
--     the user has them on hold: a built-in track (`api/data/hold-music/`),
--     `custom` (the user's upload in the bucket, its original file name) or
--     `none`.
-- phone_calls.on_hold / held_at: the user has the call on hold now (the
--   app says so), and since when.
-- phone_call_rules: a user's custom incoming call rules ("+ Add rule"): when
--   (callers: numbers and contacts; a weekly schedule; the user's numbers
--   called; NULL = any), then what (ring, forward, voicemail, message).
-- phone_call_blocking: one row per company, "Spam and blocked numbers".
-- phone_call_block_list: the company's blocked numbers and area codes, and
--   its trusted numbers (never blocked).
-- phone_blocked_calls: incoming calls the blocking stopped before anything
--   rang (no Recents row, no push), and why.
--
-- All dates are UTC. Every column change is guarded, so a database that has
-- it already is left alone. A short lock wait (3 s): behind a long
-- transaction the migration fails and is run again, instead of stalling the
-- tables' other queries. No triggers, views or generated columns: test-db
-- copies tables only.

SET SESSION lock_wait_timeout = 3;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_user_settings'
      AND COLUMN_NAME = 'forward_all') = 0,
  'ALTER TABLE phone_user_settings
     ADD COLUMN forward_all TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN forward_all_to ENUM(''voicemail'', ''number'') NOT NULL DEFAULT ''voicemail'',
     ADD COLUMN forward_all_number VARCHAR(20) NULL,
     ADD COLUMN display_number ENUM(''caller'', ''called'') NOT NULL DEFAULT ''caller'',
     ADD COLUMN unknown_show_mine TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN announce_direct TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN announce_shared TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN announce_line_name TINYINT(1) NOT NULL DEFAULT 1,
     ADD COLUMN require_pin TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN answer_pin_hash CHAR(64) NULL,
     ADD COLUMN hold_music VARCHAR(32) NOT NULL DEFAULT ''calm'',
     ADD COLUMN hold_music_url VARCHAR(512) NULL,
     ADD COLUMN hold_music_name VARCHAR(100) NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'phone_calls'
      AND COLUMN_NAME = 'on_hold') = 0,
  'ALTER TABLE phone_calls
     ADD COLUMN on_hold TINYINT(1) NOT NULL DEFAULT 0,
     ADD COLUMN held_at DATETIME(3) NULL,
     ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS phone_call_rules (
  id          BIGINT       NOT NULL AUTO_INCREMENT,
  company_id  INT          NOT NULL,
  user_id     INT          NOT NULL,
  name        VARCHAR(100) NOT NULL,
  enabled     TINYINT(1)   NOT NULL DEFAULT 1,
  -- the order rules are tried in (the first that matches decides)
  position    INT          NOT NULL DEFAULT 0,
  -- [{"number": "+1..."} | {"customerId": 123}]; NULL = any caller
  callers     JSON         NULL,
  -- working-hours JSON ({"tz", "days": [{"day", "start", "end"}]}); NULL = any time
  schedule    JSON         NULL,
  -- [telnyx_numbers.id]; NULL = any of my numbers
  number_ids  JSON         NULL,
  action      ENUM('ring', 'forward', 'voicemail', 'message') NOT NULL,
  -- E.164, with `forward`
  forward_to  VARCHAR(20)  NULL,
  -- what the caller hears, with `message`
  message     VARCHAR(500) NULL,
  created_at  DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at  DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                           ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  KEY idx_phone_call_rules_user (company_id, user_id, position),
  CONSTRAINT fk_phone_call_rules_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
  CONSTRAINT fk_phone_call_rules_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_call_blocking (
  company_id         INT          NOT NULL,
  -- calls Telnyx's caller-ID check (STIR/SHAKEN) did not vouch for
  spam_action        ENUM('ring', 'block', 'voicemail') NOT NULL DEFAULT 'ring',
  blocked_action     ENUM('announce', 'hangup', 'voicemail') NOT NULL DEFAULT 'announce',
  -- NULL = the default announcement
  announcement       VARCHAR(500) NULL,
  block_no_caller_id TINYINT(1)   NOT NULL DEFAULT 0,
  updated_by         INT          NULL,
  updated_at         DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                  ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id),
  CONSTRAINT fk_phone_call_blocking_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_call_block_list (
  id         BIGINT      NOT NULL AUTO_INCREMENT,
  company_id INT         NOT NULL,
  list       ENUM('blocked', 'trusted') NOT NULL,
  -- an E.164 number, or (blocked only) a 3-digit US or Canada area code
  pattern    VARCHAR(20) NOT NULL,
  created_by INT         NULL,
  created_at DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_call_block_list (company_id, list, pattern),
  CONSTRAINT fk_phone_call_block_list_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS phone_blocked_calls (
  id             BIGINT       NOT NULL AUTO_INCREMENT,
  company_id     INT          NOT NULL,
  number_id      INT          NULL,
  -- E.164, or what Telnyx sent (anonymous callers)
  other_number   VARCHAR(20)  NOT NULL,
  -- number, area_code, no_caller_id, spam
  reason         VARCHAR(16)  NOT NULL,
  -- announce, hangup, voicemail
  action         VARCHAR(16)  NOT NULL,
  telnyx_call_sid VARCHAR(128) NULL,
  created_at     DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  UNIQUE KEY uq_phone_blocked_calls_sid (telnyx_call_sid),
  KEY idx_phone_blocked_calls_company (company_id, created_at),
  CONSTRAINT fk_phone_blocked_calls_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
