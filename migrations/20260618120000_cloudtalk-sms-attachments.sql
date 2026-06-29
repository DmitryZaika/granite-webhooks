-- cloudtalk_sms_attachments: outbound (v1) image attachments for a cloudtalk_sms row.
CREATE TABLE cloudtalk_sms_attachments (
    id INT AUTO_INCREMENT PRIMARY KEY,
    cloudtalk_sms_id INT NOT NULL,
    content_type VARCHAR(100) NOT NULL,
    filename VARCHAR(255) NOT NULL,
    s3_key VARCHAR(512) NOT NULL,
    s3_url VARCHAR(700) NOT NULL,
    width INT NULL,
    height INT NULL,
    position INT NOT NULL DEFAULT 0,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT fk_sms_attachments_sms
        FOREIGN KEY (cloudtalk_sms_id)
        REFERENCES cloudtalk_sms(id)
        ON DELETE CASCADE,

    KEY idx_sms_attachments_sms (cloudtalk_sms_id)
);
