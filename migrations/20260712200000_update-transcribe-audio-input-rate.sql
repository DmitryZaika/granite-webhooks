-- gpt-4o-mini-transcribe input tokens are audio-dominated; audio input bills at $3.00/1M
-- (the published ~$0.003/min estimate = ~1000 audio tokens/min x $3/1M). The original
-- seed used the $1.25/1M text-input rate, understating transcription cost ~2.4x.
UPDATE ai_model_pricing
SET input_per_1m_usd = 3.000000
WHERE model = 'gpt-4o-mini-transcribe' AND effective_from = '2026-07-12';
