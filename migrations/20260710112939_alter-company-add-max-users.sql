-- Add migration script here
ALTER TABLE company ADD COLUMN max_users INT NOT NULL DEFAULT 10;