-- An event waiting for its day: kept off the calendar, in the schedule's
-- "Unscheduled" list, until someone picks the date (its dates hold when it
-- was added). quote_id: the sold quote that made it, so selling again does
-- not add it twice.
ALTER TABLE events
    ADD COLUMN unscheduled TINYINT(1) NOT NULL DEFAULT 0 AFTER all_day,
    ADD COLUMN quote_id INT NULL AFTER sale_id,
    ADD INDEX idx_events_unscheduled (unscheduled),
    ADD INDEX idx_events_quote (quote_id);
