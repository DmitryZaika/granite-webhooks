CREATE TABLE customers_emails (
    id INT PRIMARY KEY AUTO_INCREMENT,
    customer_id INT NOT NULL,
    email VARCHAR(255) NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by INT NULL,
    CONSTRAINT fk_customers_emails_customer
        FOREIGN KEY (customer_id)
        REFERENCES customers(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_customers_emails_created_by
        FOREIGN KEY (created_by)
        REFERENCES users(id)
        ON DELETE SET NULL
);

INSERT INTO customers_emails (customer_id, email)
SELECT id, email
FROM customers
WHERE email IS NOT NULL
  AND email != '';

ALTER TABLE customers
    ADD COLUMN email_id INT NULL;

UPDATE customers c
JOIN customers_emails ce ON ce.customer_id = c.id
SET c.email_id = ce.id
WHERE c.email IS NOT NULL
  AND c.email != ''
  AND c.email = ce.email;

ALTER TABLE customers
    DROP COLUMN email,
    ADD CONSTRAINT fk_customers_email_id
        FOREIGN KEY (email_id)
        REFERENCES customers_emails(id)
        ON DELETE SET NULL;
