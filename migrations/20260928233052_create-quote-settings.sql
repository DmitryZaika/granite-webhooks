-- Company defaults for new quotes (Admin > Quotes > Measurement Settings).
CREATE TABLE quote_settings (
  company_id INT NOT NULL PRIMARY KEY,
  measurement_units VARCHAR(16) NOT NULL DEFAULT 'in',
  measurement_rounding DECIMAL(6, 4) NOT NULL DEFAULT 0.0625,
  default_counter_depth DECIMAL(6, 2) NOT NULL DEFAULT 25.50,
  updated_by INT NULL,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  CONSTRAINT fk_quote_settings_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
