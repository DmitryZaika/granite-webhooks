ALTER TABLE cloudtalk_sms
  ADD COLUMN direction ENUM('inbound', 'outbound') NOT NULL DEFAULT 'inbound' AFTER text,
  ADD COLUMN status ENUM('received', 'sent', 'failed') NOT NULL DEFAULT 'received' AFTER direction,
  ADD COLUMN error_message TEXT NULL AFTER status,
  ADD COLUMN sender_user_id INT NULL AFTER agent,
  MODIFY COLUMN sender BIGINT NULL;

CREATE INDEX idx_cloudtalk_sms_company_created
  ON cloudtalk_sms (company_id, created_date);

CREATE INDEX idx_cloudtalk_sms_sender_user
  ON cloudtalk_sms (sender_user_id);

ALTER TABLE cloudtalk_sms
  ADD UNIQUE KEY uniq_cloudtalk_id_per_company (company_id, cloudtalk_id);
