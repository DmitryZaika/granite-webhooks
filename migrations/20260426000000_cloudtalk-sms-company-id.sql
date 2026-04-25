ALTER TABLE cloudtalk_sms ADD COLUMN company_id INT NULL;

CREATE INDEX idx_cloudtalk_sms_company_phones
    ON cloudtalk_sms (company_id, sender, recipient);

CREATE INDEX idx_cloudtalk_sms_created_date
    ON cloudtalk_sms (created_date);
