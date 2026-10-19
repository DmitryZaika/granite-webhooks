CREATE TABLE schedule_notification_rules (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    calendar_id INT NOT NULL,
    channel ENUM('sms', 'email') NOT NULL,
    offset_value INT NOT NULL,
    offset_unit ENUM('minutes', 'hours', 'days') NOT NULL,
    -- Store-time clock for `days` rules ("1 day before at 9:00 AM").
    send_time TIME NULL DEFAULT NULL,
    subject VARCHAR(255) NULL DEFAULT NULL,
    body TEXT NOT NULL,
    is_active TINYINT(1) NOT NULL DEFAULT 1,
    created_by INT NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL DEFAULT NULL,
    INDEX idx_schedule_notification_rules_company (company_id, deleted_at),
    CONSTRAINT fk_schedule_notification_rules_company
        FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_rules_calendar
        FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_rules_created_by
        FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE SET NULL
);

-- One row per rule, event and start time: a moved event is reminded again.
CREATE TABLE schedule_notification_sends (
    id INT AUTO_INCREMENT PRIMARY KEY,
    rule_id INT NOT NULL,
    event_id INT NOT NULL,
    start_date DATETIME NOT NULL,
    -- `skipped`: booked (or the rule made) after its send time, or no customer.
    status ENUM('sending', 'sent', 'failed', 'skipped') NOT NULL DEFAULT 'sending',
    error_message VARCHAR(255) NULL DEFAULT NULL,
    sent_at DATETIME NULL DEFAULT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_schedule_notification_send (rule_id, event_id, start_date),
    CONSTRAINT fk_schedule_notification_sends_rule
        FOREIGN KEY (rule_id) REFERENCES schedule_notification_rules(id) ON DELETE CASCADE,
    CONSTRAINT fk_schedule_notification_sends_event
        FOREIGN KEY (event_id) REFERENCES events(id) ON DELETE CASCADE
);
