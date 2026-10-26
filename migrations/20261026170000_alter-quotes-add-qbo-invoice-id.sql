-- The QuickBooks invoice made from the quote (its Create invoice button);
-- NULL until one is made.
ALTER TABLE quotes
    ADD COLUMN qbo_invoice_id VARCHAR(64) NULL DEFAULT NULL;
