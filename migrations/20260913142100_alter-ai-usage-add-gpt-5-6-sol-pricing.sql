INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    cached_input_per_1m_usd,
    output_per_1m_usd,
    effective_from
)
VALUES (
    'gpt-5.6-sol',
    4.000000,
    0.400000,
    20.000000,
    CURDATE()
);
