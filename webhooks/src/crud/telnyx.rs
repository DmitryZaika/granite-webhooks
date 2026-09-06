use crate::telnyx::schemas::TelnyxEvent;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlQueryResult;

/// Create or enrich the call-level columns. Called for `CallStatus=completed`.
pub async fn upsert_call(
    pool: &MySqlPool,
    event: &TelnyxEvent,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (account_sid, call_sid, call_sid_legacy, call_session_id, call_control_id, call_leg_id,
             connection_id, callback_source, call_status, from_number, to_number, caller_id,
             hangup_cause, hangup_source, sip_hangup_cause, call_duration, dial_call_duration,
             start_time, end_time, answered_time, call_initiated_at, timestamp, occurred_at,
             sequence_number)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            account_sid = VALUES(account_sid),
            call_sid_legacy = VALUES(call_sid_legacy),
            call_session_id = VALUES(call_session_id),
            call_control_id = VALUES(call_control_id),
            call_leg_id = VALUES(call_leg_id),
            connection_id = VALUES(connection_id),
            callback_source = VALUES(callback_source),
            call_status = VALUES(call_status),
            from_number = VALUES(from_number),
            to_number = VALUES(to_number),
            caller_id = VALUES(caller_id),
            hangup_cause = VALUES(hangup_cause),
            hangup_source = VALUES(hangup_source),
            sip_hangup_cause = VALUES(sip_hangup_cause),
            call_duration = VALUES(call_duration),
            dial_call_duration = VALUES(dial_call_duration),
            start_time = VALUES(start_time),
            end_time = VALUES(end_time),
            answered_time = VALUES(answered_time),
            call_initiated_at = VALUES(call_initiated_at),
            timestamp = VALUES(timestamp),
            occurred_at = VALUES(occurred_at),
            sequence_number = VALUES(sequence_number)
        "#,
    )
    .bind(event.account_sid.as_deref())
    .bind(event.call_sid.as_deref())
    .bind(event.call_sid_legacy.as_deref())
    .bind(event.call_session_id.as_deref())
    .bind(event.call_control_id.as_deref())
    .bind(event.call_leg_id.as_deref())
    .bind(event.connection_id.as_deref())
    .bind(event.callback_source.as_deref())
    .bind(event.call_status.as_deref())
    .bind(event.from.as_deref())
    .bind(event.to.as_deref())
    .bind(event.caller_id.as_deref())
    .bind(event.hangup_cause.as_deref())
    .bind(event.hangup_source.as_deref())
    .bind(event.sip_hangup_cause.as_deref())
    .bind(event.call_duration)
    .bind(event.dial_call_duration)
    .bind(event.start_time.as_deref())
    .bind(event.end_time.as_deref())
    .bind(event.answered_time.as_deref())
    .bind(event.call_initiated_at.as_deref())
    .bind(event.timestamp.as_deref())
    .bind(event.occurred_at.as_deref())
    .bind(event.sequence_number)
    .execute(pool)
    .await
}

/// Create or enrich the recording columns. Called for `RecordingStatus=completed`.
pub async fn upsert_recording(
    pool: &MySqlPool,
    event: &TelnyxEvent,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (call_sid, recording_sid, recording_status, recording_source, recording_channels,
             recording_duration, recording_start_time, recording_end_time, recording_url)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            recording_sid = VALUES(recording_sid),
            recording_status = VALUES(recording_status),
            recording_source = VALUES(recording_source),
            recording_channels = VALUES(recording_channels),
            recording_duration = VALUES(recording_duration),
            recording_start_time = VALUES(recording_start_time),
            recording_end_time = VALUES(recording_end_time),
            recording_url = VALUES(recording_url)
        "#,
    )
    .bind(event.call_sid.as_deref())
    .bind(event.recording_sid.as_deref())
    .bind(event.recording_status.as_deref())
    .bind(event.recording_source.as_deref())
    .bind(event.recording_channels)
    .bind(event.recording_duration)
    .bind(event.recording_start_time.as_deref())
    .bind(event.recording_end_time.as_deref())
    .bind(event.recording_url.as_deref())
    .execute(pool)
    .await
}

/// Create or enrich the transcript/conversation columns. Called for
/// `CallStatus=conversation_ended`.
pub async fn upsert_conversation(
    pool: &MySqlPool,
    event: &TelnyxEvent,
    messages_json: Option<String>,
    transcript: Option<String>,
    raw_payload: String,
) -> Result<MySqlQueryResult, sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO telnyx_calls
            (call_sid, conversation_id, assistant_id, duration_sec, llm_model, stt_model,
             tts_model_id, tts_provider, tts_voice_id, reason, messages_json, transcript,
             raw_payload)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON DUPLICATE KEY UPDATE
            conversation_id = VALUES(conversation_id),
            assistant_id = VALUES(assistant_id),
            duration_sec = VALUES(duration_sec),
            llm_model = VALUES(llm_model),
            stt_model = VALUES(stt_model),
            tts_model_id = VALUES(tts_model_id),
            tts_provider = VALUES(tts_provider),
            tts_voice_id = VALUES(tts_voice_id),
            reason = VALUES(reason),
            messages_json = VALUES(messages_json),
            transcript = VALUES(transcript),
            raw_payload = VALUES(raw_payload)
        "#,
    )
    .bind(event.call_sid.as_deref())
    .bind(event.conversation_id.as_deref())
    .bind(event.assistant_id.as_deref())
    .bind(event.duration_sec)
    .bind(event.llm_model.as_deref())
    .bind(event.stt_model.as_deref())
    .bind(event.tts_model_id.as_deref())
    .bind(event.tts_provider.as_deref())
    .bind(event.tts_voice_id.as_deref())
    .bind(event.reason.as_deref())
    .bind(messages_json)
    .bind(transcript)
    .bind(raw_payload)
    .execute(pool)
    .await
}
