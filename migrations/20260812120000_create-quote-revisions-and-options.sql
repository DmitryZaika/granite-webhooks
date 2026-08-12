-- Quote revisions, options, portal, and draft drawing for CounterGo-style quoting

ALTER TABLE quotes
  ADD COLUMN draft_drawing_json JSON NULL,
  ADD COLUMN current_step TINYINT NOT NULL DEFAULT 1,
  ADD COLUMN selected_option_id INT NULL,
  ADD COLUMN portal_token VARCHAR(64) NULL,
  ADD COLUMN signed_at DATETIME NULL,
  ADD COLUMN signature_data MEDIUMTEXT NULL,
  ADD COLUMN customer_accepted_option_id INT NULL;

CREATE TABLE quote_revisions (
  id INT AUTO_INCREMENT PRIMARY KEY,
  quote_id INT NOT NULL,
  revision_number INT NOT NULL,
  drawing_json JSON NOT NULL,
  options_json JSON NULL,
  created_by INT NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  UNIQUE KEY uq_quote_revision (quote_id, revision_number),
  CONSTRAINT fk_quote_revisions_quote
    FOREIGN KEY (quote_id) REFERENCES quotes(id),
  CONSTRAINT fk_quote_revisions_user
    FOREIGN KEY (created_by) REFERENCES users(id)
);

CREATE TABLE quote_options (
  id INT AUTO_INCREMENT PRIMARY KEY,
  quote_id INT NOT NULL,
  revision_id INT NULL,
  option_index TINYINT NOT NULL,
  product_type VARCHAR(64) NOT NULL DEFAULT 'Granite',
  color_name VARCHAR(255) NOT NULL DEFAULT '',
  edge_profile VARCHAR(64) NOT NULL DEFAULT 'flat',
  pricing_mode VARCHAR(16) NOT NULL DEFAULT 'PER_SQFT',
  sqft_price DECIMAL(12,2) NOT NULL DEFAULT 0,
  slab_price DECIMAL(12,2) NOT NULL DEFAULT 0,
  slab_sqft DECIMAL(12,2) NOT NULL DEFAULT 45,
  fab_rate DECIMAL(12,2) NOT NULL DEFAULT 0,
  edge_rate_per_lf DECIMAL(12,2) NOT NULL DEFAULT 0,
  splash_fab_rate DECIMAL(12,2) NOT NULL DEFAULT 0,
  miter_rate DECIMAL(12,2) NOT NULL DEFAULT 200,
  sealer_rate DECIMAL(12,2) NOT NULL DEFAULT 6,
  UNIQUE KEY uq_quote_option (quote_id, option_index),
  CONSTRAINT fk_quote_options_quote
    FOREIGN KEY (quote_id) REFERENCES quotes(id),
  CONSTRAINT fk_quote_options_revision
    FOREIGN KEY (revision_id) REFERENCES quote_revisions(id)
);

CREATE TABLE price_lists (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(255) NOT NULL,
  is_default TINYINT(1) NOT NULL DEFAULT 0,
  tax_rate DECIMAL(8,4) NOT NULL DEFAULT 0,
  minimum_job_charge DECIMAL(12,2) NOT NULL DEFAULT 0,
  deleted_at DATETIME NULL,
  CONSTRAINT fk_price_lists_company
    FOREIGN KEY (company_id) REFERENCES company(id)
);

CREATE UNIQUE INDEX uq_quotes_portal_token ON quotes (portal_token);
