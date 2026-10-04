CREATE TABLE user_schedule_colors (
  company_id INT NOT NULL,
  user_id INT NOT NULL,
  color VARCHAR(50) NOT NULL,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (company_id, user_id),
  KEY idx_user_schedule_colors_user (user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
