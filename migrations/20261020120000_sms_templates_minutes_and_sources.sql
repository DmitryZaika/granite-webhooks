-- Company SMS drips: minutes on top of hour_delay, and which customer sources
-- get them (JSON array of customers.source keys; NULL = only leads).
ALTER TABLE cloudtalk_sms_templates
  ADD COLUMN minute_delay INT NOT NULL DEFAULT 0 AFTER hour_delay,
  ADD COLUMN customer_sources JSON NULL AFTER minute_delay,
  DROP INDEX uk_cloudtalk_sms_templates_list_delay,
  ADD UNIQUE KEY uk_cloudtalk_sms_templates_list_delay (lead_list_id, company_id, hour_delay, minute_delay);
