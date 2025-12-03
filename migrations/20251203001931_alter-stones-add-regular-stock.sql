-- Add migration script here
ALTER TABLE stones ADD COLUMN regular_stock BOOLEAN DEFAULT FALSE;