CREATE TABLE maintenance_items (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    notes TEXT NULL,
    start_date DATETIME NOT NULL,
    rrule VARCHAR(500) NULL,
    recurrence_until DATETIME NULL,
    last_completed_at DATETIME NULL,
    created_by_user_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    CONSTRAINT fk_maintenance_items_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_items_created_by
        FOREIGN KEY (created_by_user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    INDEX idx_maintenance_items_company_deleted (company_id, deleted_at)
);

CREATE TABLE maintenance_completions (
    id INT AUTO_INCREMENT PRIMARY KEY,
    maintenance_item_id INT NOT NULL,
    completed_by_user_id INT NOT NULL,
    completed_at DATETIME NOT NULL,
    notes TEXT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_maintenance_completions_item
        FOREIGN KEY (maintenance_item_id)
        REFERENCES maintenance_items(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_completions_user
        FOREIGN KEY (completed_by_user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_completions_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    INDEX idx_maintenance_completions_item_completed (maintenance_item_id, completed_at)
);
