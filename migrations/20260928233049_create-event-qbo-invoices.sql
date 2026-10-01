-- The QuickBooks invoice shown to installers on a schedule event (one per event).
CREATE TABLE event_qbo_invoices (
  event_id INT NOT NULL,
  company_id INT NOT NULL,
  qbo_invoice_id VARCHAR(64) NOT NULL,
  qbo_invoice_number VARCHAR(64) NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (event_id),
  KEY idx_event_qbo_invoices_company (company_id),
  CONSTRAINT fk_event_qbo_invoices_event
    FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
