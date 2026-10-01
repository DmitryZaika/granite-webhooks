-- Add migration script here
ALTER TABLE stones
  ADD COLUMN bundle_location VARCHAR(255) NULL,
  ADD COLUMN delivery_date DATE NULL;