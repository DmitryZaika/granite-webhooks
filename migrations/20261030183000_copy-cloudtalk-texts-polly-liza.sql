-- Polly (44) and Liza (15) of Granite Depot of Indianapolis were moved from
-- CloudTalk to RingCentral on the Users page, which did not yet bring their
-- texts along (My Account did). Copy their CloudTalk texts, pictures and read
-- state to RingCentral as the save now does: each copy carries
-- `cloudtalk:<id>`, so nothing is copied twice.
INSERT INTO ringcentral_sms
  (ringcentral_id, sender, recipient, text, direction, status, error_message,
   idempotency_key, agent, sender_user_id, company_id, created_date,
   sender10, recipient10, phone_digits, is_echo, echo_of_sms_id, is_automated)
SELECT NULL, s.sender, s.recipient, s.text, s.direction, s.status, s.error_message,
       CONCAT('cloudtalk:', s.id), IF(s.agent IS NULL, NULL, u.ringcentral_extension_id),
       s.sender_user_id, s.company_id, s.created_date, s.sender10, s.recipient10,
       s.phone_digits, 0, NULL, s.is_automated
  FROM cloudtalk_sms s
  JOIN users u ON u.id = 44 AND u.company_id = 1 AND u.ringcentral_extension_id IS NOT NULL
 WHERE s.company_id = 1
   AND s.is_echo = 0
   AND (s.agent = '557125' OR (s.agent IS NULL AND (s.recipient10 = '3179346260' OR s.sender10 = '3179346260')))
   AND s.id NOT IN (
     SELECT CAST(SUBSTRING(r.idempotency_key, 11) AS UNSIGNED) FROM ringcentral_sms r
      WHERE r.company_id = 1 AND r.idempotency_key LIKE 'cloudtalk:%'
   );

INSERT INTO ringcentral_sms
  (ringcentral_id, sender, recipient, text, direction, status, error_message,
   idempotency_key, agent, sender_user_id, company_id, created_date,
   sender10, recipient10, phone_digits, is_echo, echo_of_sms_id, is_automated)
SELECT NULL, s.sender, s.recipient, s.text, s.direction, s.status, s.error_message,
       CONCAT('cloudtalk:', s.id), IF(s.agent IS NULL, NULL, u.ringcentral_extension_id),
       s.sender_user_id, s.company_id, s.created_date, s.sender10, s.recipient10,
       s.phone_digits, 0, NULL, s.is_automated
  FROM cloudtalk_sms s
  JOIN users u ON u.id = 15 AND u.company_id = 1 AND u.ringcentral_extension_id IS NOT NULL
 WHERE s.company_id = 1
   AND s.is_echo = 0
   AND (s.agent = '557126' OR (s.agent IS NULL AND (s.recipient10 = '3177087664' OR s.sender10 = '3177087664')))
   AND s.id NOT IN (
     SELECT CAST(SUBSTRING(r.idempotency_key, 11) AS UNSIGNED) FROM ringcentral_sms r
      WHERE r.company_id = 1 AND r.idempotency_key LIKE 'cloudtalk:%'
   );

INSERT INTO ringcentral_sms_attachments
  (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height,
   position, created_date)
SELECT r.id, a.content_type, a.filename, a.s3_key, a.s3_url, a.width, a.height,
       a.position, a.created_date
  FROM ringcentral_sms r
  JOIN cloudtalk_sms_attachments a
    ON a.cloudtalk_sms_id = CAST(SUBSTRING(r.idempotency_key, 11) AS UNSIGNED)
 WHERE r.company_id = 1
   AND r.idempotency_key LIKE 'cloudtalk:%'
   AND NOT EXISTS (
     SELECT 1 FROM ringcentral_sms_attachments x WHERE x.ringcentral_sms_id = r.id
   );

-- Without read state every copied inbound text would show as unread again.
INSERT INTO ringcentral_sms_thread_reads
  (user_id, company_id, customer_phone_digits, last_read_at)
SELECT cr.user_id, cr.company_id, cr.customer_phone_digits, cr.last_read_at
  FROM cloudtalk_sms_thread_reads cr
 WHERE cr.company_id = 1
   AND cr.customer_phone_digits IN (
     SELECT DISTINCT r.phone_digits FROM ringcentral_sms r
      WHERE r.company_id = 1 AND r.idempotency_key LIKE 'cloudtalk:%'
   )
ON DUPLICATE KEY UPDATE
  last_read_at = GREATEST(ringcentral_sms_thread_reads.last_read_at, VALUES(last_read_at));
