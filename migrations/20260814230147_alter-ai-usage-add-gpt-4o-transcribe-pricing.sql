INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    output_per_1m_usd,
    effective_from
)
VALUES (
    'gpt-4o-transcribe',
    2.50,
    10.00,
    CURDATE()
);
