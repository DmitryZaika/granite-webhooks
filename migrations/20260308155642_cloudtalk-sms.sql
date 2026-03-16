-- Add migration script here
CREATE TABLE cloudtalk_sms (
    id INT AUTO_INCREMENT PRIMARY KEY,

    cloudtalk_id INT,
    sender BIGINT NOT NULL,
    recipient BIGINT NOT NULL,

    text TEXT NOT NULL,
    agent VARCHAR(255) DEFAULT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
