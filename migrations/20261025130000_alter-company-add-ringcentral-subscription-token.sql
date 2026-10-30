-- Which token a company's RingCentral SMS subscription carries, so the API
-- can move every subscription to a new one (`common::telephony::ringcentral`,
-- the `contact-sync` job).
--
-- ringcentral_subscription_token_hash: SHA-256 (hex) of the
--   `verificationToken` the subscription at this environment's address was
--   last given. NULL: not known yet (subscriptions made before this column).
-- ringcentral_subscription_tried_at: the job's last try to renew it (UTC); a
--   failed try is retried 2 hours later.
--
-- MySQL 8.0, guarded: the file is idempotent.

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'ringcentral_subscription_token_hash') = 0,
  'ALTER TABLE company ADD COLUMN ringcentral_subscription_token_hash CHAR(64) NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'ringcentral_subscription_tried_at') = 0,
  'ALTER TABLE company ADD COLUMN ringcentral_subscription_tried_at DATETIME NULL, ALGORITHM=INSTANT',
  'DO 0');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
