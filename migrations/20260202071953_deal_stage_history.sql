CREATE TABLE deal_stage_history (
    id INT AUTO_INCREMENT PRIMARY KEY,
    deal_id BIGINT UNSIGNED NOT NULL,
    list_id INT NOT NULL,
    entered_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    exited_at TIMESTAMP NULL,
    FOREIGN KEY (deal_id) REFERENCES deals(id) ON DELETE CASCADE,
    FOREIGN KEY (list_id) REFERENCES deals_list(id)
);