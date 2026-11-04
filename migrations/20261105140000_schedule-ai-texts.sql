-- Admin > Schedule > Customer notifications > AI texts: scheduling texts
-- (sent by hand from the event) whose replies the AI reads. Only these wait
-- for the customer's answer and send follow-ups; plain scheduling texts
-- (`confirmation`) are sent and forgotten.
ALTER TABLE schedule_notification_templates
    MODIFY COLUMN kind ENUM('reminder', 'confirmation', 'question') NOT NULL DEFAULT 'reminder';

-- Plain scheduling texts no longer send follow-ups.
UPDATE schedule_notification_templates
   SET kind = 'question'
 WHERE kind = 'confirmation' AND followup_after_hours IS NOT NULL;

-- A flow step can go out the moment the installer or template maker taps
-- Completed on the job (`completed`), instead of a time before or after it.
ALTER TABLE schedule_notification_rules
    MODIFY COLUMN direction ENUM('before', 'after', 'completed') NOT NULL DEFAULT 'before';
