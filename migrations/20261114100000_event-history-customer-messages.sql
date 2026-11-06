-- The event's history now notes each automated text or email its customer
-- got (`action = 'customer_message'`, `created_by = 'Automated'`): the
-- company's flow steps (Admin > Schedule), the built-in 24-hour and 1-hour
-- reminders, and the in-home estimate booking confirmation. The api writes
-- these lines from now on; this adds them for the messages already sent, at
-- the time each went out. Booking confirmations were never stored anywhere,
-- so past ones cannot be added. Safe to run again: a line already there is
-- not added twice.

-- Flow steps: `Text sent to the customer: Template reminder (1 day before)`.
INSERT INTO event_history (event_id, company_id, action, message, created_by, created_at)
SELECT s.event_id, r.company_id, 'customer_message', line.message, 'Automated', s.sent_at
  FROM schedule_notification_sends s
  JOIN schedule_notification_rules r ON r.id = s.rule_id
  JOIN schedule_notification_templates m ON m.id = r.template_id
  JOIN events e ON e.id = s.event_id
  JOIN LATERAL (
       SELECT CONCAT(
                IF(m.channel = 'sms', 'Text', 'Email'),
                ' sent to the customer: ', TRIM(m.name), ' (',
                IF(r.direction = 'completed', 'when completed',
                   CONCAT(r.offset_value, ' ',
                          CASE r.offset_unit
                            WHEN 'minutes' THEN IF(r.offset_value = 1, 'minute', 'minutes')
                            WHEN 'hours' THEN IF(r.offset_value = 1, 'hour', 'hours')
                            ELSE IF(r.offset_value = 1, 'day', 'days')
                          END,
                          ' ', r.direction)),
                ')') AS message
       ) line
 WHERE s.status = 'sent' AND s.sent_at IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM event_history h
                    WHERE h.event_id = s.event_id AND h.action = 'customer_message'
                      AND h.message = line.message AND h.created_at = s.sent_at);

-- Built-in reminders. Which way each went (text or email) was not stored.
INSERT INTO event_history (event_id, company_id, action, message, created_by, created_at)
SELECT a.event_id, a.company_id, 'customer_message', line.message, 'Automated', a.sent_at
  FROM appointment_reminders a
  JOIN events e ON e.id = a.event_id
  JOIN LATERAL (
       SELECT CONCAT('Sent to the customer: Reminder (',
                     IF(a.reminder_kind = '24h', '1 day before', '1 hour before'),
                     ')') AS message
       ) line
 WHERE a.status = 'sent' AND a.sent_at IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM event_history h
                    WHERE h.event_id = a.event_id AND h.action = 'customer_message'
                      AND h.message = line.message AND h.created_at = a.sent_at);
