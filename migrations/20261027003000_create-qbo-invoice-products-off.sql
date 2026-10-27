-- QuickBooks products an admin turned off on the invoice rules page: a new
-- invoice's lines never get them, whatever the company's invoices teach.
CREATE TABLE qbo_invoice_products_off (
  company_id INT NOT NULL,
  item_id VARCHAR(64) NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, item_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
