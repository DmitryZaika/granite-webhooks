ALTER TABLE cloudtalk_sms_templates
  ADD COLUMN lead_list_id INT NULL AFTER body,
  ADD COLUMN hour_delay INT NULL AFTER lead_list_id,
  ADD CONSTRAINT fk_cloudtalk_sms_templates_lead_list_id
    FOREIGN KEY (lead_list_id) REFERENCES deals_list(id);

ALTER TABLE cloudtalk_sms_templates
  ADD UNIQUE KEY uk_cloudtalk_sms_templates_list_delay (lead_list_id, company_id, hour_delay);

CREATE TABLE scheduled_sms (
    id INT AUTO_INCREMENT PRIMARY KEY,
    template_id INT NOT NULL,
    deal_id INT NOT NULL,
    list_id INT NULL,
    customer_id INT NOT NULL,
    user_id INT NOT NULL,
    company_id INT NOT NULL,
    send_at DATETIME NOT NULL,
    status ENUM('pending', 'sent', 'failed', 'cancelled') NOT NULL DEFAULT 'pending',
    sent_at DATETIME NULL,
    error_message VARCHAR(500) NULL,
    provider_message_id VARCHAR(72) NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_scheduled_sms_pending (status, send_at),
    CONSTRAINT fk_scheduled_sms_template_id
      FOREIGN KEY (template_id) REFERENCES cloudtalk_sms_templates(id),
    CONSTRAINT fk_scheduled_sms_list_id
      FOREIGN KEY (list_id) REFERENCES deals_list(id)
);
