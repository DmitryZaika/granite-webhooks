-- Add migration script here
ALTER TABLE events
ADD COLUMN location VARCHAR(500) NULL AFTER description;

CREATE TABLE event_comments (
  id INT AUTO_INCREMENT PRIMARY KEY,
  event_id INT NOT NULL,
  company_id INT NOT NULL,
  content TEXT NOT NULL,
  created_by VARCHAR(255) NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  INDEX idx_event_comments_event (event_id, deleted_at)
);

CREATE TABLE event_images (
  id INT AUTO_INCREMENT PRIMARY KEY,
  event_id INT NOT NULL,
  company_id INT NOT NULL,
  url VARCHAR(1000) NOT NULL,
  created_by VARCHAR(255) NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  INDEX idx_event_images_event (event_id, deleted_at)
);