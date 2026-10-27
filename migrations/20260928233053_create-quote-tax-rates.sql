-- Tax rates a quote can pick from (Admin > Quotes > Tax Rates).
CREATE TABLE quote_tax_rates (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(120) NOT NULL,
  sequence INT NOT NULL DEFAULT 1,
  tax_pct DECIMAL(7,4) NOT NULL DEFAULT 0,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_quote_tax_rates_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_quote_tax_rates_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
