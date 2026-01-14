-- Add migration script here
-- Creating the customer_surveys table
CREATE TABLE customer_surveys (
    id INT PRIMARY KEY AUTO_INCREMENT,
    sales_rep_id INT NOT NULL,
    sales_rep_rating INT NOT NULL,
    sales_rep_comment TEXT NULL,
    technician_rating INT NOT NULL,
    technician_comment TEXT NULL,
    installation_rating INT NOT NULL,
    installation_comment TEXT NULL,
    company_id INT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,

    -- Constraints matches the style of your other tables
    CONSTRAINT fk_customer_surveys_sales_rep_id FOREIGN KEY (sales_rep_id) REFERENCES users(id),
    CONSTRAINT fk_company_customer_surveys_id FOREIGN KEY (company_id) REFERENCES company(id) ON DELETE CASCADE
);