-- Inventory Manager and Scheduling Manager: positions that open only some
-- sections of the app. A company may change a position's sections; a company
-- without a row uses the defaults in api/crates/core/src/auth/sections.rs.
INSERT INTO positions (id, name) VALUES
  (12, 'inventory_manager'),
  (13, 'scheduling_manager');

CREATE TABLE company_position_sections (
  company_id INT NOT NULL,
  position_id INT NOT NULL,
  -- Section keys, comma-separated (e.g. 'customers,schedule').
  sections VARCHAR(1000) NOT NULL,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, position_id),
  CONSTRAINT fk_company_position_sections_company FOREIGN KEY (company_id) REFERENCES company (id),
  CONSTRAINT fk_company_position_sections_position FOREIGN KEY (position_id) REFERENCES positions (id)
);
