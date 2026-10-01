-- Optional usage hours on each maintenance check (sub-item).
-- Leave NULL to use the parent equipment hours.

ALTER TABLE maintenance_sub_items
  ADD COLUMN hours_per_day DECIMAL(8, 2) NULL AFTER notes,
  ADD COLUMN days_per_month DECIMAL(8, 2) NULL AFTER hours_per_day,
  ADD COLUMN starting_hours DECIMAL(10, 2) NULL AFTER days_per_month;

-- Meter unit on starting hours (hours or miles).

ALTER TABLE maintenance_items
  ADD COLUMN usage_unit VARCHAR(16) NOT NULL DEFAULT 'hours' AFTER starting_hours;

ALTER TABLE maintenance_sub_items
  ADD COLUMN usage_unit VARCHAR(16) NULL AFTER starting_hours;
