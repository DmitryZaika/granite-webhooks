-- AI promises: what a rep told customers they would do ("I'll text you our
-- inventory link"), read from their calls, texts and emails, with the
-- message that keeps it ready to send. Only for the reps it is switched on
-- for (users.ai_promises); the first is Dema at Granite Depot of Indianapolis.
ALTER TABLE users
  ADD COLUMN ai_promises TINYINT(1) NOT NULL DEFAULT 0;

UPDATE users SET ai_promises = 1
 WHERE email = 'dema@granitedepotindy.com' AND is_deleted = 0;

-- One row per call, text or email of such a rep that the job
-- (POST /v1/jobs/promises) reads for promises. source_key names the row it
-- came from: '<provider>:<call id>' for a call ('telnyx:<id>' is Granite
-- Phone), '<table>:<id>' for a text, 'email:<id>' for an email. A call's
-- transcript is kept here.
CREATE TABLE ai_promise_scans (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NOT NULL,
  source ENUM('call', 'sms', 'email') NOT NULL,
  source_key VARCHAR(80) NOT NULL,
  happened_at DATETIME NOT NULL,
  customer_digits CHAR(10) NULL,
  customer_email VARCHAR(320) NULL,
  recording_link VARCHAR(255) NULL,
  status ENUM('pending', 'done', 'skipped', 'failed') NOT NULL DEFAULT 'pending',
  attempts TINYINT UNSIGNED NOT NULL DEFAULT 0,
  claimed_at DATETIME NULL,
  transcript MEDIUMTEXT NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uk_ai_promise_scans_source (user_id, source_key),
  KEY idx_ai_promise_scans_due (status, attempts, claimed_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- A promise. draft is the message that keeps it (channel says text or
-- email); NULL when it takes more than a message (a call back, a visit).
-- Times are UTC.
CREATE TABLE ai_promises (
  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NOT NULL,
  scan_id BIGINT UNSIGNED NOT NULL,
  source ENUM('call', 'sms', 'email') NOT NULL,
  promised_at DATETIME NOT NULL,
  customer_id INT NULL,
  customer_digits CHAR(10) NULL,
  customer_email VARCHAR(320) NULL,
  title VARCHAR(160) NOT NULL,
  said VARCHAR(500) NOT NULL,
  due_at DATETIME NULL,
  channel ENUM('sms', 'email') NULL,
  draft TEXT NULL,
  status ENUM('open', 'done', 'dismissed') NOT NULL DEFAULT 'open',
  closed_at DATETIME NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_ai_promises_user (user_id, status, due_at),
  KEY idx_ai_promises_customer (company_id, customer_id),
  KEY idx_ai_promises_digits (company_id, customer_digits),
  KEY idx_ai_promises_scan (scan_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
