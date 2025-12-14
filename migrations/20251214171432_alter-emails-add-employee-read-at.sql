-- Add migration script here
ALTER TABLE emails ADD COLUMN employee_read_at TIMESTAMP NULL;