-- Printable quote layouts (Admin > Quotes > Forms). config_json holds which
-- sections show and their text; see app/utils/quoteForms.ts in general_datebase.
CREATE TABLE quote_forms (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(160) NOT NULL,
  status ENUM('active', 'inactive') NOT NULL DEFAULT 'active',
  sequence INT NOT NULL DEFAULT 1,
  config_json JSON NOT NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_quote_forms_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_quote_forms_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
