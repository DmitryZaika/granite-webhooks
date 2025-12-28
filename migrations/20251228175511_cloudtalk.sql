-- Add migration script here
ALTER TABLE company
    ADD COLUMN cloudtalk_access_key VARCHAR(22),
    ADD COLUMN cloudtalk_access_secret VARCHAR(22);

ALTER TABLE users ADD COLUMN cloudtalk_agent_id VARCHAR(36);
