-- Setup checklist "Adjust permissions": when a Manager first opened Position
-- access (Company page). Existing companies count as done; new ones start
-- with it to do.
ALTER TABLE company ADD COLUMN permissions_reviewed_at DATETIME NULL;
UPDATE company SET permissions_reviewed_at = NOW();
