INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    cached_input_per_1m_usd,
    output_per_1m_usd,
    effective_from
)
VALUES
    ('gpt-6-luna', 0.100000, 0.010000, 0.500000, '2026-09-23'),
    ('gpt-6-sol', 2.000000, 0.200000, 10.000000, '2026-09-23');
