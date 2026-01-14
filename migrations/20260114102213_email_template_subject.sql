ALTER TABLE email_templates
ADD COLUMN template_subject VARCHAR(255) NOT NULL DEFAULT '' AFTER template_name;