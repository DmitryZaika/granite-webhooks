-- Add migration script here
ALTER TABLE deals ADD COLUMN title VARCHAR(255) NULL AFTER amount;