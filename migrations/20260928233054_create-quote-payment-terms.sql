-- Payment terms a quote can pick from (Admin > Quotes > Payment Terms).
-- The first by sequence prefills new quotes.
CREATE TABLE quote_payment_terms (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(120) NOT NULL,
  sequence INT NOT NULL DEFAULT 1,
  terms TEXT NOT NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_quote_payment_terms_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_quote_payment_terms_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
