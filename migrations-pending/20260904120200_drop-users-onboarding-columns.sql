-- Migration C of the onboarding server-state cutover. Apply only after the SPA
-- reads the user_onboarding* tables and the backfill has been re-run post-deploy.
ALTER TABLE users DROP COLUMN onboarding_progress;
ALTER TABLE users DROP COLUMN onboarding;
