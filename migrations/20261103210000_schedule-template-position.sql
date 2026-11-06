-- Admin > Schedule > Customer notifications: admins drag templates into
-- their own order (the event form's confirmations follow it). Existing
-- templates start in name order, as they were shown.
ALTER TABLE schedule_notification_templates
    ADD COLUMN position INT NOT NULL DEFAULT 0;

UPDATE schedule_notification_templates t
  JOIN (
        SELECT id,
               ROW_NUMBER() OVER (PARTITION BY company_id, kind ORDER BY name, channel, id) AS n
          FROM schedule_notification_templates
       ) ranked ON ranked.id = t.id
   SET t.position = ranked.n;
