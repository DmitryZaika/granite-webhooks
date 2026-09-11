-- Sub-second precision on the reset fence: a plain DATETIME truncates `reset_at`,
-- so events from the same second as a reset slipped past `at < reset_at` and
-- resurrected exactly what the reset deleted.
ALTER TABLE user_onboarding MODIFY reset_at DATETIME(3) NULL;
