-- Company catalog of quote extras (Admin > Quotes > Extras), e.g. Tear out.
-- Every price list offers each one at its catalog price unless the list switches
-- it off or prices it its own way (stored in the price list's extraLinks).
-- item_json: the extra as a price list stores it (name, category, flags,
-- arrOptions / arrRanges with stable ids).
CREATE TABLE quote_extras (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  sequence INT NOT NULL DEFAULT 0,
  item_json JSON NOT NULL,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_quote_extras_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_quote_extras_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
