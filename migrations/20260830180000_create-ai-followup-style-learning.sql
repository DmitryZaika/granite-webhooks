CREATE TABLE ai_followup_edit_pairs (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    deal_id INT NULL,
    followup_id INT NULL,
    channel ENUM('sms', 'email') NOT NULL,
    generated_text TEXT NOT NULL,
    final_text TEXT NOT NULL,
    angle VARCHAR(64) NULL,
    phase VARCHAR(64) NULL,
    customer_first_name VARCHAR(80) NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ai_followup_edit_pairs_user (user_id, company_id, created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE ai_followup_style_preferences (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    preferences_json JSON NOT NULL,
    sample_count INT NOT NULL DEFAULT 0,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uk_ai_followup_style_user (user_id, company_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
