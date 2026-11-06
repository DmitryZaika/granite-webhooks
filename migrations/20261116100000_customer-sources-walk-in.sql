-- A company's own source can count as a walk-in (Admin ticks it in Manage
-- Sources): its customers then show under the Customers list's Walk-in
-- filter, in "Walk-ins this month", and in the walk-in numbers of
-- Statistics and the Dashboard, next to the built-in walk-in (`check-in`).
ALTER TABLE customer_sources
  ADD COLUMN is_walkin TINYINT(1) NOT NULL DEFAULT 0 AFTER is_system;
