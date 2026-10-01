ALTER TABLE sms_flow_steps
  ADD COLUMN channel ENUM('sms', 'email') NOT NULL DEFAULT 'sms' AFTER position,
  ADD COLUMN subject VARCHAR(300) NULL AFTER custom_text;

ALTER TABLE sms_flow_enrollments
  MODIFY COLUMN customer_phone_digits BIGINT NULL,
  ADD COLUMN customer_email VARCHAR(255) NULL AFTER customer_id;
