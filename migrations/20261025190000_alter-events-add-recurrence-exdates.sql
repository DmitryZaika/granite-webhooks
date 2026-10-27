-- The days a repeating event skips (TimeTree's EXDATE): comma-separated
-- YYYY-MM-DD in the shop's time zone.
ALTER TABLE events
    ADD COLUMN recurrence_exdates TEXT NULL AFTER recurrence_until;
