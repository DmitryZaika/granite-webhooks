-- Add migration script here
UPDATE ai_model_pricing
SET input_per_1m_usd        = 2.00,
    output_per_1m_usd       = 12.00,
    image_input_per_1m_usd  = 2.00,
    image_output_per_1m_usd = 120.00,
    per_image_usd           = 0.134
WHERE model = 'gemini-3-pro-image-preview';