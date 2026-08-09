CREATE TABLE maintenance_due_digest_sends (
    id INT AUTO_INCREMENT PRIMARY KEY,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    due_on DATE NOT NULL,
    sent_at DATETIME NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uniq_maintenance_due_digest_user_day (user_id, due_on),
    INDEX idx_maintenance_due_digest_company_day (company_id, due_on),
    CONSTRAINT fk_maintenance_due_digest_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_due_digest_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);