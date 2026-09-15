-- Telnyx call-progress events.
--
-- We receive three event kinds on /telnyx/event-progress and enrich one row
-- per call:
--   * CallStatus=completed       -> base call columns (upsert_call)
--   * RecordingStatus=completed  -> recording columns (upsert_recording)
--   * CallStatus=conversation_ended -> transcript/conversation columns
--
-- `call_sid` is shared by all three and is the natural dedupe key.
CREATE TABLE IF NOT EXISTS telnyx_calls (
  id INT AUTO_INCREMENT PRIMARY KEY,

  -- Identity / routing
  account_sid VARCHAR(64) NULL,
  call_sid VARCHAR(255) NOT NULL,
  call_sid_legacy VARCHAR(255) NULL,
  call_session_id VARCHAR(255) NULL,
  call_control_id VARCHAR(255) NULL,
  call_leg_id VARCHAR(255) NULL,
  connection_id VARCHAR(255) NULL,
  callback_source VARCHAR(255) NULL,

  -- Call completion
  call_status VARCHAR(64) NULL,
  from_number VARCHAR(64) NULL,
  to_number VARCHAR(64) NULL,
  caller_id VARCHAR(255) NULL,
  hangup_cause VARCHAR(64) NULL,
  hangup_source VARCHAR(64) NULL,
  sip_hangup_cause VARCHAR(64) NULL,
  call_duration INT NULL,
  dial_call_duration INT NULL,
  start_time VARCHAR(64) NULL,
  end_time VARCHAR(64) NULL,
  answered_time VARCHAR(64) NULL,
  call_initiated_at VARCHAR(64) NULL,
  timestamp VARCHAR(64) NULL,
  occurred_at VARCHAR(64) NULL,
  sequence_number BIGINT NULL,

  -- Recording
  recording_sid VARCHAR(64) NULL,
  recording_status VARCHAR(64) NULL,
  recording_source VARCHAR(64) NULL,
  recording_channels INT NULL,
  recording_duration INT NULL,
  recording_start_time VARCHAR(64) NULL,
  recording_end_time VARCHAR(64) NULL,
  recording_url TEXT NULL,

  -- AI conversation / transcript
  conversation_id VARCHAR(64) NULL,
  assistant_id VARCHAR(64) NULL,
  duration_sec INT NULL,
  llm_model VARCHAR(255) NULL,
  stt_model VARCHAR(255) NULL,
  tts_model_id VARCHAR(255) NULL,
  tts_provider VARCHAR(255) NULL,
  tts_voice_id VARCHAR(255) NULL,
  reason VARCHAR(255) NULL,
  messages_json LONGTEXT NULL,
  transcript LONGTEXT NULL,
  raw_payload LONGTEXT NULL,

  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,

  UNIQUE KEY uniq_telnyx_call_sid (call_sid),
  KEY idx_telnyx_calls_conversation_id (conversation_id),
  KEY idx_telnyx_calls_call_session_id (call_session_id),
  KEY idx_telnyx_calls_created_at (created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
