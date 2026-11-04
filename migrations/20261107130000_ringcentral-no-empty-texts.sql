-- RingCentral texts with no words and no files only ever showed as
-- "No preview available.": the blank echo of a picture-only send (stored a
-- second time without its RingCentral id) and copies of CloudTalk pictures
-- CloudTalk never handed over. Neither carries anything to show.
DELETE s FROM ringcentral_sms s
 WHERE TRIM(REPLACE(REPLACE(REPLACE(s.text, '\r', ''), '\n', ''), '\t', '')) = ''
   AND NOT EXISTS (
     SELECT 1 FROM ringcentral_sms_attachments a WHERE a.ringcentral_sms_id = s.id
   );
