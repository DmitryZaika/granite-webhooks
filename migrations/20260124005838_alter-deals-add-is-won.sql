-- Add migration script here
ALTER TABLE deals ADD COLUMN is_won INT DEFAULT NULL;