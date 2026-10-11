-- Commissions: rules an admin builds (Admin > Quotes > Commissions), the company's
-- base pay and payroll day, and what each signed quote earns its rep.
CREATE TABLE commission_rules (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(120) NOT NULL,
  applies VARCHAR(20) NOT NULL,
  rate_type VARCHAR(10) NOT NULL,
  rate DECIMAL(12,4) NOT NULL DEFAULT 0,
  priority INT NOT NULL DEFAULT 0,
  stop TINYINT(1) NOT NULL DEFAULT 0,
  materials_json JSON NULL,
  colors_json JSON NULL,
  level_min DECIMAL(6,2) NULL,
  level_max DECIMAL(6,2) NULL,
  line_match VARCHAR(120) NOT NULL DEFAULT '',
  threshold DECIMAL(12,2) NULL,
  active TINYINT(1) NOT NULL DEFAULT 1,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_commission_rules_company (company_id, deleted_at, priority),
  CONSTRAINT fk_commission_rules_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE commission_settings (
  company_id INT NOT NULL PRIMARY KEY,
  base_weekly DECIMAL(10,2) NOT NULL DEFAULT 0,
  payroll_weekday TINYINT NOT NULL DEFAULT 5,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  CONSTRAINT fk_commission_settings_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- One row per signed quote: what its rep earns and whether the customer paid
-- (from the matching QuickBooks invoice).
CREATE TABLE quote_commissions (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  quote_id INT NOT NULL,
  user_id INT NULL,
  customer_id INT NOT NULL,
  option_id VARCHAR(64) NULL,
  quote_total DECIMAL(12,2) NOT NULL DEFAULT 0,
  commission_amount DECIMAL(12,2) NOT NULL DEFAULT 0,
  parts_json JSON NULL,
  qbo_invoice_id VARCHAR(64) NULL,
  invoice_total DECIMAL(12,2) NULL,
  invoice_balance DECIMAL(12,2) NULL,
  status VARCHAR(20) NOT NULL DEFAULT 'no_invoice',
  paid_date DATE NULL,
  payroll_date DATE NULL,
  paid_out_at DATETIME NULL,
  signed_at DATETIME NULL,
  synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_quote_commissions_quote (quote_id),
  KEY idx_quote_commissions_company (company_id, user_id, payroll_date),
  CONSTRAINT fk_quote_commissions_quote
    FOREIGN KEY (quote_id) REFERENCES quotes(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
