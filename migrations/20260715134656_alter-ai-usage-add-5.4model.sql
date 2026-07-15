INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    output_per_1m_usd,
    effective_from
)
VALUES (
    'gpt-5.4-mini-2026-03-17',
    0.75,
    4.50,
    CURDATE()
);