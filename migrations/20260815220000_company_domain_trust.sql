-- Company email deliverability check cache (SPF / DMARC / MX / blocklists)
CREATE TABLE company_domain_trust (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  domain VARCHAR(255) NOT NULL,
  reputation_score DECIMAL(6, 2) NULL,
  risk_level ENUM('GOOD', 'REVIEW', 'HIGH_RISK', 'UNKNOWN') NOT NULL DEFAULT 'UNKNOWN',
  reputation_reasons JSON NULL,
  checked_at DATETIME NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  UNIQUE KEY uniq_company_domain_trust_company (company_id),
  INDEX idx_company_domain_trust_domain_checked (domain, checked_at),
  CONSTRAINT fk_company_domain_trust_company
    FOREIGN KEY (company_id) REFERENCES company (id)
    ON DELETE CASCADE
);
