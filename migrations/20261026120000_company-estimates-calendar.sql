-- The calendar the deal page's Schedule estimate button books on (Admin >
-- Schedule); NULL means the built-in Estimates calendar.
ALTER TABLE company
    ADD COLUMN estimates_calendar_id INT NULL DEFAULT NULL;
