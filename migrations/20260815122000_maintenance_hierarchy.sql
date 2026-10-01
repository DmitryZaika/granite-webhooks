-- Hierarchical maintenance: equipment + checks + conditions + starting hours.
-- Main item (e.g. Saw): usage profile + optional starting hour meter.
-- Sub-item (e.g. Change oil): belongs to a main item.
-- Condition: hours and/or calendar days; completing a check resets all its conditions.

ALTER TABLE maintenance_items
  MODIFY COLUMN start_date DATETIME NULL,
  MODIFY COLUMN notes TEXT NULL,
  ADD COLUMN hours_per_day DECIMAL(8, 2) NULL AFTER notes,
  ADD COLUMN days_per_month DECIMAL(8, 2) NULL AFTER hours_per_day,
  ADD COLUMN starting_hours DECIMAL(10, 2) NULL AFTER days_per_month;

CREATE TABLE maintenance_sub_items (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    maintenance_item_id INT NOT NULL,
    name VARCHAR(255) NOT NULL,
    notes TEXT NULL,
    last_completed_at DATETIME NULL,
    created_by_user_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    CONSTRAINT fk_maintenance_sub_items_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_sub_items_parent
        FOREIGN KEY (maintenance_item_id)
        REFERENCES maintenance_items(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_sub_items_created_by
        FOREIGN KEY (created_by_user_id)
        REFERENCES users(id)
        ON DELETE CASCADE,
    INDEX idx_maintenance_sub_items_parent_deleted (maintenance_item_id, deleted_at),
    INDEX idx_maintenance_sub_items_company_deleted (company_id, deleted_at)
);

CREATE TABLE maintenance_conditions (
    id INT AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    maintenance_sub_item_id INT NOT NULL,
    condition_type ENUM('hours', 'days') NOT NULL,
    interval_value DECIMAL(10, 2) NOT NULL,
    next_due_date DATE NOT NULL,
    hours_done DECIMAL(10, 2) NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    deleted_at DATETIME NULL,
    CONSTRAINT fk_maintenance_conditions_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_maintenance_conditions_sub_item
        FOREIGN KEY (maintenance_sub_item_id)
        REFERENCES maintenance_sub_items(id)
        ON DELETE CASCADE,
    INDEX idx_maintenance_conditions_sub_deleted (maintenance_sub_item_id, deleted_at),
    INDEX idx_maintenance_conditions_due (company_id, next_due_date, deleted_at)
);

ALTER TABLE maintenance_completions
  ADD COLUMN maintenance_sub_item_id INT NULL AFTER maintenance_item_id,
  ADD CONSTRAINT fk_maintenance_completions_sub_item
      FOREIGN KEY (maintenance_sub_item_id)
      REFERENCES maintenance_sub_items(id)
      ON DELETE CASCADE,
  ADD INDEX idx_maintenance_completions_sub_completed (maintenance_sub_item_id, completed_at);
