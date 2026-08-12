CREATE TABLE sms_flows (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NULL,
    name VARCHAR(255) NOT NULL,
    created_by INT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NULL ON UPDATE CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    INDEX idx_sms_flows_company (company_id, user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE sms_flow_steps (
    id INT AUTO_INCREMENT PRIMARY KEY,
    flow_id INT NOT NULL,
    position INT NOT NULL,
    template_id INT NULL,
    custom_text TEXT NULL,
    delay_hours INT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_sms_flow_steps_flow FOREIGN KEY (flow_id)
        REFERENCES sms_flows (id) ON DELETE CASCADE,
    INDEX idx_sms_flow_steps_flow (flow_id, position)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE sms_flow_enrollments (
    id INT AUTO_INCREMENT PRIMARY KEY,
    flow_id INT NOT NULL,
    company_id INT NOT NULL,
    customer_phone_digits BIGINT NOT NULL,
    customer_id INT NULL,
    user_id INT NOT NULL,
    status ENUM('active','paused','completed','cancelled','stopped_by_reply','failed')
        NOT NULL DEFAULT 'active',
    current_step_position INT NOT NULL DEFAULT 1,
    anchor_at DATETIME NOT NULL,
    next_send_at DATETIME NULL,
    attempt_count INT NOT NULL DEFAULT 0,
    error_message VARCHAR(255) NULL,
    started_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NULL ON UPDATE CURRENT_TIMESTAMP,
    INDEX idx_sms_flow_enrollments_due (status, next_send_at),
    INDEX idx_sms_flow_enrollments_phone (company_id, customer_phone_digits, status)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
