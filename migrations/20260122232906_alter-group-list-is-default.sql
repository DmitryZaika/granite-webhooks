-- Add migration script here
ALTER TABLE groups_list ADD COLUMN is_default BOOLEAN NOT NULL DEFAULT FALSE;