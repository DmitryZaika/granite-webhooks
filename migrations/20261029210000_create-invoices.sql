-- Invoices the CRM keeps itself, for companies that do not invoice through
-- QuickBooks: numbered per company, made from a quote or by hand, emailed to
-- the customer as a PDF from the CRM's composer.
--
-- status: 'draft' (not sent yet), 'sent', 'paid' or 'void'.
-- line_items: [{"description", "qty", "unit_price", "amount"}], in order; a
-- line without a unit price is text only. subtotal and total are kept in
-- step with the lines by the API (total = subtotal + tax).
CREATE TABLE invoices (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  invoice_number INT NOT NULL,
  customer_id INT NULL,
  quote_id INT NULL,
  status VARCHAR(10) NOT NULL DEFAULT 'draft',
  invoice_date DATE NOT NULL,
  due_date DATE NULL,
  bill_to_name VARCHAR(255) NOT NULL,
  bill_to_email VARCHAR(255) NULL,
  project_address VARCHAR(500) NULL,
  line_items JSON NOT NULL,
  subtotal DECIMAL(12,2) NOT NULL DEFAULT 0,
  tax DECIMAL(12,2) NOT NULL DEFAULT 0,
  total DECIMAL(12,2) NOT NULL DEFAULT 0,
  customer_memo TEXT NULL,
  private_note TEXT NULL,
  sent_at TIMESTAMP NULL DEFAULT NULL,
  paid_at TIMESTAMP NULL DEFAULT NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY uq_invoices_company_number (company_id, invoice_number),
  KEY idx_invoices_company_customer (company_id, customer_id),
  KEY idx_invoices_company_quote (company_id, quote_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
