-- Add migration script here
ALTER TABLE company ADD COLUMN deleted_at TIMESTAMP NULL;