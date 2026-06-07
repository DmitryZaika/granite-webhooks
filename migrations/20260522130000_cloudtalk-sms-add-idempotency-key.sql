ALTER TABLE cloudtalk_sms
  ADD COLUMN idempotency_key VARCHAR(36) NULL AFTER error_message;
