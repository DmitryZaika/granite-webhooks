-- The day's route map (and route buttons) on the schedule, per calendar
-- (Admin > Schedule > Calendars > Day map). NULL is the default: on for the
-- Installations and Templates calendars, off for the others; an admin's
-- choice saves 0 or 1.
ALTER TABLE calendars ADD COLUMN shows_day_map TINYINT(1) NULL DEFAULT NULL;
