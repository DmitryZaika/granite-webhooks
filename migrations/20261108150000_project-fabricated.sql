-- When the shop marked the job fabricated (the project's Fabricated button).
-- A fabricated job waits in In Fabrication until its installation is on the
-- calendar, then it is Install Scheduled.
ALTER TABLE projects ADD COLUMN fabricated_at DATETIME NULL;
