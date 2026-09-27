CREATE TABLE delayed_emails (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    customer_id INT NULL,
    thread_id VARCHAR(255) NULL,
    recipient_emails TEXT NOT NULL,
    send_at DATETIME NOT NULL,
    cancel_if_customer_messages TINYINT(1) NOT NULL DEFAULT 0,
    baseline_at DATETIME NOT NULL,
    status ENUM('pending', 'sending', 'sent', 'cancelled', 'failed') NOT NULL DEFAULT 'pending',
    payload_json MEDIUMTEXT NOT NULL,
    sent_at DATETIME NULL,
    error_message VARCHAR(500) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_delayed_emails_pending (status, send_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE delayed_sms (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    user_id INT NOT NULL,
    customer_id INT NULL,
    phone_digits VARCHAR(32) NOT NULL,
    provider ENUM('cloudtalk', 'ringcentral') NOT NULL,
    send_at DATETIME NOT NULL,
    cancel_if_customer_messages TINYINT(1) NOT NULL DEFAULT 0,
    baseline_at DATETIME NOT NULL,
    status ENUM('pending', 'sending', 'sent', 'cancelled', 'failed') NOT NULL DEFAULT 'pending',
    payload_json MEDIUMTEXT NOT NULL,
    sent_at DATETIME NULL,
    error_message VARCHAR(500) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_delayed_sms_pending (status, send_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
