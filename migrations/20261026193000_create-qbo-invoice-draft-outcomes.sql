-- What became of a quote's invoice draft when it was created in QuickBooks:
-- per draft line kept, changed (to `saved_value`, its numbers as {0}, {1}...)
-- or removed, and the class, CC and product the invoice went out with. The
-- next drafts follow what recent invoices keep doing.
CREATE TABLE qbo_invoice_draft_outcomes (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NULL,
  quote_id INT NULL,
  qbo_invoice_id VARCHAR(64) NULL,
  -- 'line', 'class', 'cc' or 'item'
  field VARCHAR(10) NOT NULL,
  -- A line's text with its numbers as '#': the same line on every quote.
  draft_shape VARCHAR(255) NULL,
  draft_text VARCHAR(1000) NULL,
  -- Lines: 'kept', 'changed' or 'removed'. Class and CC: 'none',
  -- 'sales_rep' or 'value'. Product: 'value'.
  outcome VARCHAR(10) NOT NULL,
  saved_value VARCHAR(1000) NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_qbo_invoice_draft_outcomes_field (company_id, field, draft_shape, id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- How the company's recent QuickBooks invoices set class, CC and product,
-- read again at most daily: what a company that just connected starts from.
CREATE TABLE qbo_invoice_scans (
  company_id INT NOT NULL PRIMARY KEY,
  scanned_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  stats_json JSON NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
