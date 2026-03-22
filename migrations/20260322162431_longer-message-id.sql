-- Add migration script here
ALTER TABLE emails
MODIFY COLUMN message_id VARCHAR(500);
