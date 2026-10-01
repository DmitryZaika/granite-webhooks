ALTER TABLE sms_flow_steps ADD COLUMN delay_minutes INT NOT NULL DEFAULT 0;

UPDATE sms_flow_steps SET delay_minutes = delay_hours * 60;

ALTER TABLE sms_flow_steps ALTER COLUMN delay_minutes DROP DEFAULT;

ALTER TABLE sms_flow_steps DROP COLUMN delay_hours;
