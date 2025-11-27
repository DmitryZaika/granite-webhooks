-- Add migration script here
ALTER TABLE emails DROP FOREIGN KEY emails_ibfk_1;


ALTER TABLE emails
    ADD COLUMN thread_id CHAR(36),
    ADD COLUMN sender_user_id INT NULL,
    ADD COLUMN receiver_user_id INT NULL,
    ADD COLUMN sender_email VARCHAR(255),
    ADD COLUMN receiver_email VARCHAR(255),
    MODIFY COLUMN message_id VARCHAR(72) NULL,
    DROP COLUMN user_id,

    ADD CONSTRAINT fk_sender_user
        FOREIGN KEY (sender_user_id) REFERENCES users(id)
        ON DELETE SET NULL,

    ADD CONSTRAINT fk_receiver_user
        FOREIGN KEY (receiver_user_id) REFERENCES users(id)
        ON DELETE SET NULL;


ALTER TABLE users ADD COLUMN email_signature VARCHAR(1000) NULL;
