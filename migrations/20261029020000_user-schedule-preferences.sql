-- Each user's own schedule settings (Schedule > settings button): the day
-- their weeks start on (NULL follows the company's choice) and 12- or
-- 24-hour times.
ALTER TABLE users
    ADD COLUMN schedule_week_starts_monday TINYINT(1) NULL,
    ADD COLUMN schedule_time_24h TINYINT(1) NOT NULL DEFAULT 0;
