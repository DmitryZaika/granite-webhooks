UPDATE ai_model_pricing
SET input_per_1m_usd = 0.200000,
    cached_input_per_1m_usd = 0.020000,
    output_per_1m_usd = 1.200000
WHERE model = 'gpt-5.6-luna'
  AND effective_from = '2026-07-12';

DELETE FROM ai_model_pricing
WHERE model = 'gpt-5.6-luna'
  AND effective_from != '2026-07-12';
