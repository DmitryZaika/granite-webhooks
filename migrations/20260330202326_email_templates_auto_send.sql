ALTER TABLE email_templates
  ADD COLUMN lead_group_id INT NULL,
  ADD COLUMN hour_delay INT NULL,
  ADD COLUMN show_template TINYINT(1) NOT NULL DEFAULT 1;

ALTER TABLE email_templates
  ADD UNIQUE KEY uk_email_templates_group (lead_group_id, company_id);
