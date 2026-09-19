CREATE TABLE IF NOT EXISTS user_template_order_mode (
  user_id INT NOT NULL,
  ordering_type ENUM('sms', 'email') NOT NULL,
  mode ENUM('automatic', 'custom') NOT NULL DEFAULT 'automatic',
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, ordering_type)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS user_template_order (
  user_id INT NOT NULL,
  ordering_type ENUM('sms', 'email') NOT NULL,
  template_id INT NOT NULL,
  custom_position INT NOT NULL,
  PRIMARY KEY (user_id, ordering_type, template_id),
  INDEX idx_user_template_order_list (user_id, ordering_type, custom_position)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS user_template_usage (
  user_id INT NOT NULL,
  template_type ENUM('cloudtalk_sms', 'ringcentral_sms', 'email') NOT NULL,
  template_id INT NOT NULL,
  usage_count INT NOT NULL DEFAULT 1,
  last_used_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (user_id, template_type, template_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
