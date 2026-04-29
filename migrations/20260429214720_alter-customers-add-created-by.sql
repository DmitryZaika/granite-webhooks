-- Add migration script here
ALTER TABLE customers ADD COLUMN created_by VARCHAR(100) NULL;