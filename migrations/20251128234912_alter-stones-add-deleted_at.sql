-- Add migration script here
ALTER TABLE stones ADD COLUMN deleted_at TIMESTAMP NULL DEFAULT NULL;