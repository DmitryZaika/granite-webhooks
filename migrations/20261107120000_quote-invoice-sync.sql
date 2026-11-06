-- Admin › Quotes › Automation "Automatically synchronize quote with
-- invoices": the default of "Keep in sync with the quote" when an invoice is
-- created from a quote. Off for every company.
ALTER TABLE company ADD COLUMN quote_invoice_sync TINYINT(1) NOT NULL DEFAULT 0;

-- A quote's invoice kept in step with the quote. When the quote's invoice
-- lines change, the quote page rebuilds the invoice from the quote: on its
-- own while every invoice is unpaid and unsent, after asking otherwise.
CREATE TABLE quote_invoice_sync (
  quote_id INT NOT NULL PRIMARY KEY,
  company_id INT NOT NULL,
  -- The quote option invoiced.
  option_index INT NOT NULL,
  -- 'qbo' (QuickBooks invoices) or 'own' (`invoices`).
  kind VARCHAR(8) NOT NULL,
  -- QuickBooks: the invoice ids in order, comma-separated (several for a
  -- split). Own: the invoice's id.
  invoice_ids VARCHAR(1000) NOT NULL,
  -- What rebuilding needs, kept as the CRM sends it: a split's share of
  -- each part per invoice, and each invoice's priced lines by part.
  detail_json JSON NOT NULL,
  -- The quote's invoice lines when the invoice was last made or synced
  -- (a hash): a different one means the quote changed.
  source_hash VARCHAR(64) NOT NULL,
  enabled TINYINT(1) NOT NULL DEFAULT 1,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  KEY idx_quote_invoice_sync_company (company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
