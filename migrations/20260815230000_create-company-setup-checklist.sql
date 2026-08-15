-- Per-company setup checklist: manual overrides + cached expensive auto-checks
CREATE TABLE company_setup_checklist (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  manual_overrides JSON NULL,
  auto_cache JSON NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY uniq_company_setup_checklist_company (company_id),
  CONSTRAINT fk_company_setup_checklist_company
    FOREIGN KEY (company_id) REFERENCES company (id)
    ON DELETE CASCADE
);
