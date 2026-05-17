CREATE TABLE cloudtalk_contacts (
    id INT AUTO_INCREMENT PRIMARY KEY,

    customer_id INT NOT NULL,
    company_id INT NOT NULL,
    cloudtalk_id INT NOT NULL,

    last_synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        ON UPDATE CURRENT_TIMESTAMP,
    last_error TEXT NULL,

    UNIQUE KEY uniq_customer (customer_id),
    KEY idx_company_cloudtalk (company_id, cloudtalk_id)
);
