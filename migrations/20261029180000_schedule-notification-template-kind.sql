-- A template is a reminder (or follow-up) that flows send at a time before or
-- after the appointment, or a confirmation employees send by hand with
-- "Send a confirmation now". Only reminders go in flows.
ALTER TABLE schedule_notification_templates
    ADD COLUMN kind ENUM('reminder', 'confirmation') NOT NULL DEFAULT 'reminder' AFTER name;

-- The templates no flow sends so far are the confirmations.
UPDATE schedule_notification_templates m
   SET m.kind = 'confirmation'
 WHERE NOT EXISTS (
        SELECT 1
          FROM schedule_notification_rules r
          JOIN schedule_notification_flows f ON f.id = r.flow_id AND f.deleted_at IS NULL
         WHERE r.template_id = m.id AND r.deleted_at IS NULL);
