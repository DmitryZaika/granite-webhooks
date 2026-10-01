CREATE TABLE user_dashboard_widgets (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    widget_id VARCHAR(64) NOT NULL,
    position INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uq_user_dashboard_widgets_user_company_widget (user_id, company_id, widget_id),
    INDEX idx_user_dashboard_widgets_user_company (user_id, company_id, position),
    CONSTRAINT fk_user_dashboard_widgets_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_user_dashboard_widgets_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);
