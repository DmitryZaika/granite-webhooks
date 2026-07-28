CREATE TABLE appointment_reminders (
    id INT AUTO_INCREMENT PRIMARY KEY,
    event_id INT NOT NULL,
    company_id INT NOT NULL,
    customer_id INT NOT NULL,
    calendar_slug VARCHAR(50) NOT NULL,
    reminder_kind VARCHAR(30) NOT NULL,
    send_at DATETIME NOT NULL,
    status ENUM('pending', 'sent', 'failed', 'cancelled') NOT NULL DEFAULT 'pending',
    sent_at DATETIME NULL,
    error_message VARCHAR(255) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_appointment_reminder (event_id, reminder_kind),
    INDEX idx_appointment_reminders_due (status, send_at),
    INDEX idx_appointment_reminders_calendar_slug (calendar_slug)
);
