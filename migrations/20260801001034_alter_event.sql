ALTER TABLE events
    ADD COLUMN rrule VARCHAR(500) NULL AFTER calendar_id,
    ADD COLUMN recurrence_until DATETIME NULL AFTER rrule;
