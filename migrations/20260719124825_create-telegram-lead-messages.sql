CREATE TABLE telegram_lead_messages (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    customer_id INT NOT NULL,
    company_id INT NOT NULL,
    chat_id BIGINT NOT NULL,
    message_id INT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMP NULL,
    UNIQUE KEY uq_telegram_lead_messages_chat_message (chat_id, message_id),
    CONSTRAINT fk_telegram_lead_messages_customer
        FOREIGN KEY (customer_id)
        REFERENCES customers(id)
        ON DELETE CASCADE,
    CONSTRAINT fk_telegram_lead_messages_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);
