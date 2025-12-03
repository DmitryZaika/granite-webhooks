-- Add migration script here
-- Add migration script here
ALTER TABLE sinks ADD COLUMN regular_stock BOOLEAN DEFAULT FALSE;
ALTER TABLE faucets ADD COLUMN regular_stock BOOLEAN DEFAULT FALSE;