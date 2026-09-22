CREATE TABLE customer_sources (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    source_key VARCHAR(255) NOT NULL,
    label VARCHAR(255) NOT NULL,
    is_system TINYINT(1) NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,
    CONSTRAINT fk_company_customer_sources_id FOREIGN KEY (company_id) REFERENCES company(id),
    INDEX idx_customer_sources_company (company_id)
);
