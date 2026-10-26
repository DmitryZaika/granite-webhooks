-- RingCentral's id-less, blank push for a sent MMS was stored as its own
-- outbound text and showed as an empty "No preview available." bubble.
DELETE s FROM ringcentral_sms s
WHERE s.direction = 'outbound'
  AND s.ringcentral_id IS NULL
  AND s.sender IS NOT NULL
  AND s.sender_user_id IS NULL
  AND TRIM(s.text) = ''
  AND NOT EXISTS (
    SELECT 1 FROM ringcentral_sms_attachments a WHERE a.ringcentral_sms_id = s.id
  );
