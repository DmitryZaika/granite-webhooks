CREATE TABLE widget_analytics_events (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    widget_id VARCHAR(64) NOT NULL,
    action VARCHAR(32) NOT NULL,
    target VARCHAR(64) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_widget_analytics_events_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_widget_analytics_events_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    INDEX idx_widget_analytics_created (created_at),
    INDEX idx_widget_analytics_used (action, widget_id, target),
    INDEX idx_widget_analytics_added (action, widget_id)
);
