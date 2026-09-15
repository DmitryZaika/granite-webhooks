-- AssemblyAI Universal-3.5 Pro pre-recorded: $0.21/hr = $0.0035/min.
-- Transcription cost uses per_minute_usd (audio_seconds), not token rates.
INSERT INTO ai_model_pricing (
    model,
    input_per_1m_usd,
    output_per_1m_usd,
    per_minute_usd,
    effective_from
)
VALUES (
    'universal-3-5-pro',
    NULL,
    NULL,
    0.003500,
    CURDATE()
);
