-- Notes on a sold project, left from its schedule events: a template maker
-- tells the installers or the sales rep what they found on site. Every event
-- of the sale (template, install) shows the same notes.
CREATE TABLE sale_project_notes (
  id INT NOT NULL AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  sale_id INT NOT NULL,
  -- `installers`, `sales_rep` or `everyone`.
  audience VARCHAR(16) NOT NULL DEFAULT 'everyone',
  content TEXT NOT NULL,
  created_by INT NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  KEY idx_sale_project_notes_sale (company_id, sale_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
