ALTER TABLE message_feedback
    ADD COLUMN user_id INT NULL DEFAULT NULL,
    ADD COLUMN company_id INT NULL DEFAULT NULL,
    ADD COLUMN status VARCHAR(32) NOT NULL DEFAULT 'escalated',
    ADD COLUMN manual_section_id VARCHAR(120) NULL,
    ADD COLUMN proposed_manual_change TEXT NULL;

ALTER TABLE message_feedback
    MODIFY COLUMN company_id INT NULL DEFAULT NULL;

CREATE TABLE crm_manual_overrides (
    id INT AUTO_INCREMENT PRIMARY KEY,
    section_id VARCHAR(120) NOT NULL,
    content TEXT NOT NULL,
    feedback_id INT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    UNIQUE KEY uq_crm_manual_section (section_id)
);
