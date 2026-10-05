-- Defaults for emailing quotes (Admin > Quotes > Email Templates).
-- user_id NULL = shared with everyone in the company; set = only that user sees it.
CREATE TABLE quote_email_templates (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NULL,
  name VARCHAR(120) NOT NULL,
  reply_to VARCHAR(255) NULL,
  from_name VARCHAR(120) NULL,
  subject VARCHAR(255) NOT NULL,
  body TEXT NOT NULL,
  sequence INT NOT NULL DEFAULT 1,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_quote_email_templates_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_quote_email_templates_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
  CONSTRAINT fk_quote_email_templates_user
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
