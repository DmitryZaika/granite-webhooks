-- Add migration script here
ALTER TABLE emails MODIFY COLUMN subject VARCHAR(255) NULL;
