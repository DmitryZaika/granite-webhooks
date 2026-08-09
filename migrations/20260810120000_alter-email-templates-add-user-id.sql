ALTER TABLE email_templates
  ADD COLUMN user_id INT NULL AFTER company_id,
  ADD COLUMN created_by INT NULL AFTER user_id,
  ADD CONSTRAINT fk_email_templates_user
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  ADD CONSTRAINT fk_email_templates_created_by
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL,
  ADD KEY idx_email_templates_user_list (company_id, user_id, deleted_at);
