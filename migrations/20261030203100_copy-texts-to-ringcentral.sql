-- Every user on RingCentral has their CloudTalk texts there, as a save of
-- their line now does (`routes::sms::moves`): the texts they sent, those
-- tagged with an agent id of theirs, and those on a number of theirs, tagged
-- with their extension, with pictures and read state. Each copy's key names
-- its original (`cloudtalk:<id>`), so nothing is copied twice.
INSERT INTO ringcentral_sms
  (sender, recipient, text, direction, status, error_message, idempotency_key, agent,
   sender_user_id, company_id, created_date, sender10, recipient10, phone_digits,
   is_echo, echo_of_sms_id, is_automated)
SELECT s.sender, s.recipient, s.text, s.direction, s.status, s.error_message,
       CONCAT('cloudtalk:', s.id), m.extension, s.sender_user_id, s.company_id, s.created_date,
       s.sender10, s.recipient10, s.phone_digits, 0, NULL, s.is_automated
  FROM cloudtalk_sms s
  JOIN (
    SELECT p.id, MIN(p.extension) AS extension
      FROM (
        SELECT c.id, TRIM(u.ringcentral_extension_id) AS extension
          FROM cloudtalk_sms c
          JOIN users u ON u.id = c.sender_user_id AND u.company_id = c.company_id
         WHERE c.is_echo = 0 AND u.is_deleted = 0
           AND COALESCE(TRIM(u.ringcentral_extension_id), '') <> ''
        UNION
        SELECT c.id, TRIM(u.ringcentral_extension_id)
          FROM cloudtalk_sms c
          JOIN user_sms_identities i
            ON i.company_id = c.company_id AND i.provider = 'cloudtalk' AND i.kind = 'agent'
           AND i.value = c.agent
          JOIN users u ON u.id = i.user_id
         WHERE c.is_echo = 0 AND u.is_deleted = 0
           AND COALESCE(TRIM(u.ringcentral_extension_id), '') <> ''
        UNION
        SELECT c.id, TRIM(u.ringcentral_extension_id)
          FROM cloudtalk_sms c
          JOIN user_sms_identities i
            ON i.company_id = c.company_id AND i.kind = 'line' AND i.value = c.recipient10
          JOIN users u ON u.id = i.user_id
         WHERE c.is_echo = 0 AND u.is_deleted = 0
           AND COALESCE(TRIM(u.ringcentral_extension_id), '') <> ''
        UNION
        SELECT c.id, TRIM(u.ringcentral_extension_id)
          FROM cloudtalk_sms c
          JOIN user_sms_identities i
            ON i.company_id = c.company_id AND i.kind = 'line' AND i.value = c.sender10
          JOIN users u ON u.id = i.user_id
         WHERE c.is_echo = 0 AND u.is_deleted = 0
           AND COALESCE(TRIM(u.ringcentral_extension_id), '') <> ''
      ) p
     GROUP BY p.id
  ) m ON m.id = s.id
 WHERE CONCAT('cloudtalk:', s.id) NOT IN (
   SELECT r.idempotency_key FROM ringcentral_sms r WHERE r.idempotency_key LIKE 'cloudtalk:%'
 );

INSERT INTO ringcentral_sms_attachments
  (ringcentral_sms_id, content_type, filename, s3_key, s3_url, width, height,
   position, created_date)
SELECT r.id, a.content_type, a.filename, a.s3_key, a.s3_url, a.width, a.height,
       a.position, a.created_date
  FROM ringcentral_sms r
  JOIN cloudtalk_sms_attachments a
    ON a.cloudtalk_sms_id = CAST(SUBSTRING(r.idempotency_key, 11) AS UNSIGNED)
 WHERE r.idempotency_key LIKE 'cloudtalk:%'
   AND NOT EXISTS (
     SELECT 1 FROM ringcentral_sms_attachments x WHERE x.ringcentral_sms_id = r.id
   );

-- Without read state every copied inbound text would show as unread again.
INSERT INTO ringcentral_sms_thread_reads
  (user_id, company_id, customer_phone_digits, last_read_at)
SELECT cr.user_id, cr.company_id, cr.customer_phone_digits, cr.last_read_at
  FROM cloudtalk_sms_thread_reads cr
 WHERE (cr.company_id, cr.customer_phone_digits) IN (
     SELECT DISTINCT r.company_id, r.phone_digits FROM ringcentral_sms r
      WHERE r.idempotency_key LIKE 'cloudtalk:%'
   )
ON DUPLICATE KEY UPDATE
  last_read_at = GREATEST(ringcentral_sms_thread_reads.last_read_at, VALUES(last_read_at));
