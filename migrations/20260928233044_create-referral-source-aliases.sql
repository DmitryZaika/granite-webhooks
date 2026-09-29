-- Remembers how an unrecognized lead referral_source was resolved, so the same
-- value doesn't call GPT again and the review email goes out at most once a day.
CREATE TABLE referral_source_aliases (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    company_id INT NOT NULL,
    -- Normalized value as sent: lowercase, '-' and '_' as spaces, single spaces.
    raw_key VARCHAR(255) NOT NULL,
    -- Value to store instead; NULL while unresolved.
    resolved_value VARCHAR(255) NULL,
    -- 1 = GPT was certain, 0 = GPT was unsure, NULL = GPT could not be asked.
    ai_certain TINYINT(1) NULL,
    reason VARCHAR(500) NULL,
    times_seen INT NOT NULL DEFAULT 0,
    last_notified_at TIMESTAMP NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    CONSTRAINT fk_company_referral_source_aliases_id FOREIGN KEY (company_id) REFERENCES company(id),
    UNIQUE KEY uq_referral_source_aliases_company_key (company_id, raw_key)
);
