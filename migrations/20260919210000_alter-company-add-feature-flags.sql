ALTER TABLE company
  ADD COLUMN quickbooks_enabled TINYINT(1) NOT NULL DEFAULT 1,
  ADD COLUMN signwell_api_key VARCHAR(255) NULL;
