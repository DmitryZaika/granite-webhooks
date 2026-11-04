-- Admin › Quotes › Automation "Create invoice from quote": marking a quote
-- sold opens its QuickBooks invoice, filled from the sold option, to confirm.
-- Off for every company.
ALTER TABLE company ADD COLUMN auto_invoice TINYINT(1) NOT NULL DEFAULT 0;
