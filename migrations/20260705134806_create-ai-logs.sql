CREATE TABLE ai_usage_logs (
  id INT AUTO_INCREMENT PRIMARY KEY,
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  component_key VARCHAR(64) NOT NULL,
  model VARCHAR(64) NOT NULL,
  prompt_tokens INT NOT NULL DEFAULT 0,
  completion_tokens INT NOT NULL DEFAULT 0,
  total_tokens INT NOT NULL DEFAULT 0,
  estimated_cost_usd DECIMAL(10, 6) NOT NULL DEFAULT 0,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT fk_ai_usage_logs_user FOREIGN KEY (user_id) REFERENCES users (id),
  CONSTRAINT fk_ai_usage_logs_company FOREIGN KEY (company_id) REFERENCES company (id),
  INDEX idx_ai_usage_logs_company_component (company_id, component_key),
  INDEX idx_ai_usage_logs_created_at (created_at)
);
