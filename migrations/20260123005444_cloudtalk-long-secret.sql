-- Add migration script here
ALTER TABLE company MODIFY COLUMN cloudtalk_access_secret VARCHAR(50);
