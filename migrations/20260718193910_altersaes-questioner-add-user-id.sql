ALTER TABLE sales_questionnaires
    ADD COLUMN user_id INT NULL,
    ADD CONSTRAINT fk_sales_questionnaires_user
        FOREIGN KEY (user_id)
        REFERENCES users(id)
        ON DELETE CASCADE;
