CREATE TABLE email_template_attachments (
  id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  template_id INT NOT NULL,
  content_type VARCHAR(64) NOT NULL,
  content_subtype VARCHAR(64) NOT NULL DEFAULT '',
  filename VARCHAR(255) NOT NULL,
  url VARCHAR(2048) NOT NULL,
  position INT NOT NULL DEFAULT 0,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  CONSTRAINT fk_eta_template
    FOREIGN KEY (template_id) REFERENCES email_templates(id)
);
CREATE INDEX idx_eta_template_id ON email_template_attachments(template_id);