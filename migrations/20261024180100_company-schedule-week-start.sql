-- The schedule's weeks start on Monday instead of Sunday (Admin > Schedule).
ALTER TABLE company
    ADD COLUMN schedule_week_starts_monday TINYINT(1) NOT NULL DEFAULT 0;
