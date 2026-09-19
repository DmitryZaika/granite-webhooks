-- Migration C of the onboarding server-state cutover.
--
-- `users.onboarding` is a TIMESTAMP ("finished the first-run journey"), not a
-- blob; `user_onboarding.completed_at` is its replacement. Dropping the column
-- without carrying that timestamp over would make every already-onboarded user
-- look brand new and re-trigger first-run guidance, so the carry-over runs here
-- rather than in a separate script that could be forgotten or run out of order.
-- Rich `onboarding_progress` blobs (tasks, modules, ledger) were migrated by the
-- TypeScript backfill before this wave; the two statements below only guarantee
-- the journey row that every onboarded user must have.

-- 1. Users with no journey row at all. LEFT JOIN, not ON DUPLICATE KEY: a live
--    row is never touched, so this is idempotent by construction.
INSERT INTO user_onboarding (user_id, company_id, completed_at)
SELECT u.id, u.company_id, u.onboarding
  FROM users u
  LEFT JOIN user_onboarding o ON o.user_id = u.id
 WHERE u.is_deleted = 0
   AND u.company_id IS NOT NULL
   AND u.onboarding IS NOT NULL
   AND o.user_id IS NULL;

-- 2. Rows created by the new client before this ran. `reset_at IS NULL` keeps a
--    deliberate reset from being undone — a reset user is meant to look new.
UPDATE user_onboarding o
  JOIN users u ON u.id = o.user_id
   SET o.completed_at = u.onboarding
 WHERE o.completed_at IS NULL
   AND o.reset_at IS NULL
   AND u.onboarding IS NOT NULL;

ALTER TABLE users DROP COLUMN onboarding_progress;
ALTER TABLE users DROP COLUMN onboarding;
