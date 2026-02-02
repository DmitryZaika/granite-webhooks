CREATE TABLE deal_activities (
  id INT AUTO_INCREMENT PRIMARY KEY,
  deal_id BIGINT UNSIGNED NOT NULL,
  company_id INT NOT NULL,
  name VARCHAR(255) NOT NULL,
  deadline DATETIME NULL,
  priority ENUM('low', 'medium', 'high') NOT NULL DEFAULT 'medium',
  is_completed TINYINT(1) NOT NULL DEFAULT 0,
  completed_at DATETIME NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at DATETIME NULL,
  CONSTRAINT fk_deal_activities_deal FOREIGN KEY (deal_id) REFERENCES deals(id),
  CONSTRAINT fk_deal_activities_company FOREIGN KEY (company_id) REFERENCES company(id)
);