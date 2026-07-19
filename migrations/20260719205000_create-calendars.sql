CREATE TABLE calendars (
  id INT AUTO_INCREMENT PRIMARY KEY,
  company_id INT NOT NULL,
  name VARCHAR(100) NOT NULL,
  color VARCHAR(50) NOT NULL DEFAULT 'blue',
  created_by INT NULL,
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  deleted_at TIMESTAMP NULL,
  INDEX idx_calendars_company (company_id, deleted_at),
  CONSTRAINT fk_calendars_company FOREIGN KEY (company_id) REFERENCES company(id),
  CONSTRAINT fk_calendars_created_by FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);

ALTER TABLE events
ADD COLUMN calendar_id INT NULL AFTER sale_id,
ADD INDEX idx_events_calendar (calendar_id),
ADD CONSTRAINT fk_events_calendar FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE SET NULL;
