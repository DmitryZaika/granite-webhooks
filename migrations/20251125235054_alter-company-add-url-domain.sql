-- Add migration script here
ALTER TABLE company 
    ADD COLUMN logo_url VARCHAR(255) NULL,
    ADD COLUMN domain VARCHAR(255) NULL;