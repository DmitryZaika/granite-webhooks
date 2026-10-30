-- Add-on entitlements and the Walk-in system user.
--
-- company.plan_source: 'legacy' (every existing company: all add-ons, today's
--                      behaviour) or 'subscription' (a self-serve or moved
--                      company: an add-on works only while it has an active
--                      company_addons row).
-- company_addons:      the add-ons a subscription company has: bought
--                      ('subscription', with its Stripe item) or granted free
--                      by a superuser ('comp'). Active while started_at has
--                      passed and ended_at is NULL or in the future.
-- users.is_system:     an account no person signs in with (the Walk-in kiosk
--                      user): never billable. Backfilled for the Walk-in users
--                      company creation made: name 'Walk-in', email walk-in@...,
--                      no positions.
--
-- Each ADD COLUMN is skipped when the column exists and CREATE TABLE uses IF NOT
-- EXISTS. Safe to run more than once. ALGORITHM=INSTANT: metadata-only change.
-- lock_wait_timeout makes the ALTER fail instead of queueing queries behind its
-- metadata lock. A failed run leaves a success = 0 row in _sqlx_migrations that
-- blocks sqlx: DELETE it, then re-run.

SET SESSION lock_wait_timeout = 30;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'company'
      AND COLUMN_NAME = 'plan_source') > 0,
  'DO 0',
  'ALTER TABLE company ADD COLUMN plan_source ENUM(''legacy'',''subscription'') NOT NULL DEFAULT ''legacy'', ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

CREATE TABLE IF NOT EXISTS company_addons (
  company_id INT NOT NULL,
  addon_key VARCHAR(32) NOT NULL,
  source ENUM('subscription','comp') NOT NULL,
  stripe_subscription_item_id VARCHAR(255) NULL,
  granted_by INT NULL,
  started_at DATETIME NOT NULL,
  ended_at DATETIME NULL,
  PRIMARY KEY (company_id, addon_key, source),
  CONSTRAINT fk_company_addons_company FOREIGN KEY (company_id) REFERENCES company (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

SET @sql := IF(
  (SELECT COUNT(*) FROM information_schema.COLUMNS
    WHERE TABLE_SCHEMA = DATABASE()
      AND TABLE_NAME = 'users'
      AND COLUMN_NAME = 'is_system') > 0,
  'DO 0',
  'ALTER TABLE users ADD COLUMN is_system TINYINT(1) NOT NULL DEFAULT 0, ALGORITHM=INSTANT');
PREPARE stmt FROM @sql; EXECUTE stmt; DEALLOCATE PREPARE stmt;

UPDATE users u
   SET u.is_system = 1
 WHERE u.is_system = 0
   AND u.name = 'Walk-in'
   AND u.email LIKE 'walk-in@%'
   AND NOT EXISTS (SELECT 1 FROM users_positions up WHERE up.user_id = u.id);
