-- Admin › Automations: marking a quote sold adds its template and
-- installation to the schedule's Unscheduled list. Off for every company
-- except company 1, which already works this way.
ALTER TABLE company ADD COLUMN auto_unscheduled_jobs TINYINT(1) NOT NULL DEFAULT 0;
UPDATE company SET auto_unscheduled_jobs = 1 WHERE id = 1;
