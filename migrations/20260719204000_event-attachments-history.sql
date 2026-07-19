CREATE TABLE event_attachments (
  id INT AUTO_INCREMENT PRIMARY KEY,
  event_id INT NOT NULL,
  company_id INT NOT NULL,
  url VARCHAR(1000) NOT NULL,
  name VARCHAR(500) NOT NULL,
  kind ENUM('image', 'document') NOT NULL DEFAULT 'image',
  created_by VARCHAR(255) NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  INDEX idx_event_attachments_event (event_id, deleted_at)
);

CREATE TABLE event_history (
  id INT AUTO_INCREMENT PRIMARY KEY,
  event_id INT NOT NULL,
  company_id INT NOT NULL,
  action VARCHAR(50) NOT NULL,
  message VARCHAR(255) NOT NULL,
  created_by VARCHAR(255) NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_event_history_event (event_id, created_at)
);
