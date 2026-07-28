CREATE TABLE cloudtalk_sms_templates (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    body TEXT NOT NULL,
    position INT NOT NULL DEFAULT 0,
    created_by INT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,
    CONSTRAINT fk_cloudtalk_sms_templates_company
        FOREIGN KEY (company_id) REFERENCES company(id),
    CONSTRAINT fk_cloudtalk_sms_templates_user
        FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    CONSTRAINT fk_cloudtalk_sms_templates_created_by
        FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL,
    KEY idx_cloudtalk_sms_templates_user_list (company_id, user_id, deleted_at, position)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
