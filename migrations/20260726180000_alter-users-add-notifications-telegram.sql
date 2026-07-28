ALTER TABLE users
  ADD COLUMN notifications_telegram_id BIGINT NULL AFTER telegram_id,
  ADD COLUMN telegram_sms_notifications TINYINT(1) NOT NULL DEFAULT 1,
  ADD COLUMN telegram_email_notifications TINYINT(1) NOT NULL DEFAULT 1,
  ADD COLUMN telegram_activity_notifications TINYINT(1) NOT NULL DEFAULT 1;

CREATE INDEX idx_users_notifications_telegram_id ON users (notifications_telegram_id);
