-- Add migration script here
ALTER TABLE emails
    ADD COLUMN deal_id BIGINT UNSIGNED NULL,
    ADD CONSTRAINT fk_emails_deal_id
        FOREIGN KEY (deal_id) REFERENCES deals(id)