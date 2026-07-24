INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    output_per_1m_usd,
    effective_from
)
VALUES (
    'gemini-3-pro-image-preview',
    1.25,
    3.75,
    CURDATE()
);