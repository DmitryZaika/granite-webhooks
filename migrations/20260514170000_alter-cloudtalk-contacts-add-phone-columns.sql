ALTER TABLE cloudtalk_contacts
    ADD COLUMN phone_e164_1 VARCHAR(20) NULL AFTER cloudtalk_id,
    ADD COLUMN phone_e164_2 VARCHAR(20) NULL AFTER phone_e164_1,
    ADD INDEX idx_company_phone1 (company_id, phone_e164_1),
    ADD INDEX idx_company_phone2 (company_id, phone_e164_2);
