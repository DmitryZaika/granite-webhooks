-- Add migration script here
ALTER TABLE customer_surveys ADD COLUMN installer_id INT NULL;
ALTER TABLE customer_surveys ADD CONSTRAINT fk_customer_surveys_installer_id FOREIGN KEY (installer_id) REFERENCES users(id);
