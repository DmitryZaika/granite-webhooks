-- What a rep changed in a quote's invoice draft before creating it in
-- QuickBooks (a line reworded, removed or added): the edits the next drafts
-- learn from when the same change keeps coming back.
CREATE TABLE qbo_invoice_draft_edits (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NULL,
  quote_id INT NULL,
  qbo_invoice_id VARCHAR(64) NULL,
  -- 'changed', 'removed' or 'added'
  kind VARCHAR(10) NOT NULL,
  draft_text VARCHAR(1000) NULL,
  saved_text VARCHAR(1000) NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_qbo_invoice_draft_edits_company (company_id, id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
