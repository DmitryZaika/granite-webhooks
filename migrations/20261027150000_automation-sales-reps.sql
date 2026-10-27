-- Company drips can be limited to some sales reps: JSON array of users.id
-- whose deals get the template (NULL or empty = every rep). Two drips may
-- now share a list and delay when their reps differ, so the (list, delay)
-- keys stop being unique; lead_list_id keeps an index for its foreign key.
ALTER TABLE email_templates
  ADD COLUMN sales_rep_ids JSON NULL AFTER hour_delay,
  ADD KEY idx_email_templates_list_delay (lead_list_id, company_id, hour_delay),
  DROP INDEX uk_email_templates_list_delay;

ALTER TABLE cloudtalk_sms_templates
  ADD COLUMN sales_rep_ids JSON NULL AFTER customer_sources,
  ADD KEY idx_cloudtalk_sms_templates_list_delay (lead_list_id, company_id, hour_delay, minute_delay),
  DROP INDEX uk_cloudtalk_sms_templates_list_delay;
