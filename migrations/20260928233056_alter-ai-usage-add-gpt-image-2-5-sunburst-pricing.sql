-- AI Design (api.ai-design) moved from gemini-3-pro-image-preview to OpenAI.
-- Text output is not billed for image models; image output is.
INSERT INTO ai_model_pricing
    (model, input_per_1m_usd, cached_input_per_1m_usd, output_per_1m_usd,
     image_input_per_1m_usd, image_output_per_1m_usd, per_image_usd, per_minute_usd, effective_from)
VALUES
    ('gpt-image-2.5-sunburst', 5.000000, 1.250000, NULL, 8.000000, 30.000000, NULL, NULL, '2026-10-02');
