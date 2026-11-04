-- How a quote was invoiced: the invoices a split made (the percent each
-- charged of each part of the quote: stone, area, ...) and the class each
-- part's lines got. A company's next split of a quote with the same parts
-- starts from its newest one, and a rep's lines from the classes they last
-- gave each part.
CREATE TABLE qbo_invoice_splits (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  user_id INT NULL,
  quote_id INT NULL,
  -- The QuickBooks invoice ids, in order, comma-separated.
  qbo_invoice_ids VARCHAR(1000) NOT NULL,
  invoice_count INT NOT NULL,
  -- The quote's parts, sorted and comma-separated ("area,stone").
  part_keys VARCHAR(255) NOT NULL,
  -- [{"label": "Deposit", "shares": {"stone": 50, "area": 50}}, ...]; empty
  -- for one invoice.
  invoices_json JSON NOT NULL,
  -- {"stone": null, "area": "Cabinets:Tania"}: null is the invoice's class.
  classes_json JSON NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  KEY idx_qbo_invoice_splits_company (company_id, invoice_count, id),
  KEY idx_qbo_invoice_splits_user (company_id, user_id, id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
