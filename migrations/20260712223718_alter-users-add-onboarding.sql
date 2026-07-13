-- Add migration script here
ALTER TABLE users ADD COLUMN onboarding TIMESTAMP NULL DEFAULT NULL;
-- Optionally, set onboarding to current time for all existing users
UPDATE users SET onboarding = CURRENT_TIMESTAMP;
