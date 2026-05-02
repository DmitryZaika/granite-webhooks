-- Add migration script here
CREATE TABLE customers_history (
      id INT AUTO_INCREMENT PRIMARY KEY,
      customer_id INT NOT NULL,
      reassigned_by VARCHAR(255) NULL,
      reassigned_to VARCHAR(255) NULL,
      updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
      FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE
);


