-- Add migration script here
ALTER TABLE users ADD COLUMN pined_bar BOOLEAN DEFAULT FALSE NOT NULL; 