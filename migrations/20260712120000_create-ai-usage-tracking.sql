CREATE TABLE ai_usage_log (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    feature VARCHAR(50) NOT NULL,
    api_kind ENUM('chat', 'image', 'transcription') NOT NULL,
    model VARCHAR(100) NOT NULL,
    user_id INT NULL,
    company_id INT NULL,
    input_tokens INT UNSIGNED NULL,
    cached_input_tokens INT UNSIGNED NULL,
    output_tokens INT UNSIGNED NULL,
    image_input_tokens INT UNSIGNED NULL,
    image_output_tokens INT UNSIGNED NULL,
    image_count SMALLINT UNSIGNED NULL,
    audio_seconds DECIMAL(10, 2) NULL,
    cost_usd DECIMAL(12, 6) NOT NULL DEFAULT 0,
    success TINYINT(1) NOT NULL DEFAULT 1,
    error_message VARCHAR(500) NULL,
    duration_ms INT UNSIGNED NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    INDEX idx_ai_usage_log_feature_created (feature, created_at),
    INDEX idx_ai_usage_log_user_created (user_id, created_at),
    INDEX idx_ai_usage_log_company_created (company_id, created_at),
    INDEX idx_ai_usage_log_created (created_at)
);

CREATE TABLE ai_model_pricing (
    id INT AUTO_INCREMENT PRIMARY KEY,
    model VARCHAR(100) NOT NULL,
    input_per_1m_usd DECIMAL(12, 6) NULL,
    cached_input_per_1m_usd DECIMAL(12, 6) NULL,
    output_per_1m_usd DECIMAL(12, 6) NULL,
    image_input_per_1m_usd DECIMAL(12, 6) NULL,
    image_output_per_1m_usd DECIMAL(12, 6) NULL,
    per_image_usd DECIMAL(12, 6) NULL,
    per_minute_usd DECIMAL(12, 6) NULL,
    effective_from DATE NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE KEY uq_ai_model_pricing_model_effective (model, effective_from)
);

INSERT INTO ai_model_pricing
    (model, input_per_1m_usd, cached_input_per_1m_usd, output_per_1m_usd,
     image_input_per_1m_usd, image_output_per_1m_usd, per_image_usd, per_minute_usd, effective_from)
VALUES
    ('gpt-5.6-luna',           1.000000, 0.100000, 6.000000,  NULL,      NULL,      NULL,     NULL,     '2026-07-12'),
    ('gpt-image-1.5',          5.000000, 1.250000, 10.000000, 8.000000,  32.000000, 0.034000, NULL,     '2026-07-12'),
    ('gpt-image-1',            5.000000, 1.250000, NULL,      10.000000, 40.000000, 0.042000, NULL,     '2026-07-12'),
    ('gpt-4o-mini-transcribe', 1.250000, NULL,     5.000000,  NULL,      NULL,      NULL,     0.003000, '2026-07-12');
