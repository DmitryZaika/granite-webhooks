-- Add migration script here
ALTER TABLE instructions ADD COLUMN public BOOLEAN DEFAULT FALSE;