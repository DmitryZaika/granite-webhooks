CREATE TABLE scheduled_emails (
    id INT AUTO_INCREMENT PRIMARY KEY,
    template_id INT NOT NULL,
    deal_id INT NOT NULL,
    customer_id INT NOT NULL,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    send_at DATETIME NOT NULL,
    status ENUM('pending', 'sent', 'failed', 'cancelled') NOT NULL DEFAULT 'pending',
    sent_at DATETIME NULL,
    error_message VARCHAR(500) NULL,
    message_id VARCHAR(72) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_scheduled_emails_pending (status, send_at)
);