-- Extra options the online quote offers the customer (Admin > Quotes >
-- Customer Quote), e.g. "Stone tear out". When any is on, the customer
-- quote gets an Extras step; the picked ones go on the quote as extras.
-- Every company starts with Laminate tear out, Stone tear out and 10 year
-- sealer, all off; the admin page adds them the first time it is opened.
CREATE TABLE online_quote_options (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(120) NOT NULL,
  sequence INT NOT NULL DEFAULT 1,
  enabled TINYINT(1) NOT NULL DEFAULT 0,
  created_by INT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL DEFAULT NULL,
  KEY idx_online_quote_options_company (company_id, deleted_at, sequence),
  CONSTRAINT fk_online_quote_options_company
    FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
