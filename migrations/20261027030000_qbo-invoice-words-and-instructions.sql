-- The per-product switches are gone from the admin page: instructions replace them.
DROP TABLE IF EXISTS qbo_invoice_products_off;

-- The words of the quote an invoice was created from (its draft lines), so
-- what reps do to drafts is learned per wording: "Fabrication is left out
-- when the quote has cabinets", the product reps pick for such quotes.
CREATE TABLE qbo_invoice_draft_words (
  company_id INT NOT NULL,
  qbo_invoice_id VARCHAR(64) NOT NULL,
  -- Distinct lowercase words, space separated.
  words TEXT NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, qbo_invoice_id),
  KEY idx_qbo_invoice_draft_words_created (company_id, created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- What an admin tells the AI about the company's invoices, and the exact
-- rules the AI made of it (every new invoice draft follows them).
CREATE TABLE qbo_invoice_instructions (
  company_id INT NOT NULL PRIMARY KEY,
  instructions TEXT NOT NULL,
  -- {"rules": [...], "not_understood": [...]}
  rules_json JSON NOT NULL,
  updated_by INT NULL,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
