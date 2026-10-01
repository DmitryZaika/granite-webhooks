-- A row hides one calendar (and its events) from one user's schedule. Set in Admin > Schedule.
CREATE TABLE calendar_hidden_users (
  calendar_id INT NOT NULL,
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (calendar_id, user_id),
  KEY idx_calendar_hidden_users_user (company_id, user_id),
  CONSTRAINT fk_calendar_hidden_users_calendar
    FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE,
  CONSTRAINT fk_calendar_hidden_users_user
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
