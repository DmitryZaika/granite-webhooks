CREATE TABLE email_attachments (
    id INT AUTO_INCREMENT PRIMARY KEY,
    email_id INT NOT NULL,
    content_type VARCHAR(100) NOT NULL,
    content_subtype VARCHAR(100),
    filename VARCHAR(255) NOT NULL,
    url TEXT NOT NULL,

    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT fk_email_attachments_email
        FOREIGN KEY (email_id)
        REFERENCES emails(id)
        ON DELETE CASCADE
);
