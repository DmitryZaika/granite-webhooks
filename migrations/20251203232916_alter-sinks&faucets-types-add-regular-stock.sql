-- Add migration script here
-- Add migration script here
ALTER TABLE sink_type ADD COLUMN regular_stock BOOLEAN DEFAULT FALSE;
ALTER TABLE faucet_type ADD COLUMN regular_stock BOOLEAN DEFAULT FALSE;