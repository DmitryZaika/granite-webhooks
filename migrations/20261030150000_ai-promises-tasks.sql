-- AI promises as a short list of general tasks that close themselves:
-- - the quote work a customer gives the rep is one task per customer (kind
--   'quote', e.g. "Update the kitchen quote"), with what they asked for as
--   its details, one ask per line, instead of a row per ask; it closes once
--   one of their quotes is saved or emailed after the last ask;
-- - crm_action is what the rep does in the CRM that finishes a promise:
--   'contract' (a document sent to sign), 'schedule' (an event for the
--   customer on the schedule), 'call' (a call of 30 seconds or more with
--   them); the job closes the promise once that happens.
ALTER TABLE ai_promises
  ADD COLUMN crm_action ENUM('contract', 'schedule', 'call') NULL AFTER kind,
  ADD COLUMN details TEXT NULL AFTER said;

-- The open quote changes saved before, a row per ask, become one task per
-- customer: the newest row of each takes all their asks as its details, and
-- the others, now in it, are closed.
UPDATE ai_promises p
  JOIN (SELECT MAX(id) AS task_id, GROUP_CONCAT(title ORDER BY id SEPARATOR '\n') AS asks
          FROM ai_promises
         WHERE kind = 'quote' AND status = 'open'
         GROUP BY user_id, company_id,
                  COALESCE(CONCAT('c', customer_id), customer_digits, customer_email, CONCAT('p', id))) t
    ON t.task_id = p.id
   SET p.title = 'Update the quote', p.details = t.asks;

UPDATE ai_promises p
  JOIN (SELECT user_id, company_id,
               COALESCE(CONCAT('c', customer_id), customer_digits, customer_email, CONCAT('p', id)) AS who,
               MAX(id) AS task_id
          FROM ai_promises
         WHERE kind = 'quote' AND status = 'open'
         GROUP BY user_id, company_id, who) t
    ON t.user_id = p.user_id AND t.company_id = p.company_id
   AND t.who = COALESCE(CONCAT('c', p.customer_id), p.customer_digits, p.customer_email, CONCAT('p', p.id))
   AND p.id <> t.task_id
   SET p.status = 'done', p.closed_at = UTC_TIMESTAMP()
 WHERE p.kind = 'quote' AND p.status = 'open';
