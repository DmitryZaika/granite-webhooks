-- Self-serve signup rate limits.
--
-- signup_intents idx_signup_intents_ip (ip, created_at): `POST /v1/public/signups`
--                               counts the signups one client IP started in the
--                               last hour (at most 10). The per-email count uses
--                               idx_signup_intents_email.
--
-- Skipped when already done. Safe to run more than once. lock_wait_timeout
-- makes the ALTER fail instead of queueing queries behind its metadata lock. A
-- failed run leaves a success = 0 row in _sqlx_migrations that blocks sqlx:
-- DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.STATISTICS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'signup_intents'
      AND INDEX_NAME = 'idx_signup_intents_ip') > 0,
  'DO 0',
  'ALTER TABLE signup_intents ADD KEY idx_signup_intents_ip (ip, created_at), ALGORITHM=INPLACE, LOCK=NONE');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;
