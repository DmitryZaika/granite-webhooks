CREATE TABLE groups_list (
    id INT AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP DEFAULT NULL,
    is_displayed BOOLEAN DEFAULT TRUE,
    CONSTRAINT fk_groups_list_company_id FOREIGN KEY (company_id) REFERENCES company(id)
);


ALTER TABLE deals_list 
    ADD COLUMN group_id INT NULL,
    ADD CONSTRAINT fk_deals_list_group_id FOREIGN KEY (group_id) REFERENCES groups_list(id);
