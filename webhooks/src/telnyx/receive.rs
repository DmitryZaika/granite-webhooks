use crate::libs::constants::{BAD_REQUEST, ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::telnyx::api::{ConversationMessage, api_messages_to_text, fetch_conversation_messages};
use crate::telnyx::schemas::{TelnyxEvent, transcript_text};
use axum::body::Bytes;
use axum::extract::State;
use lambda_http::tracing;
use sqlx::MySqlPool;

/// Single entrypoint for Telnyx call-progress events.
///
/// Telnyx fans several event kinds out to `/telnyx/event-progress`. We persist
/// each kind into the same `telnyx_calls` row (keyed by `CallSid`) with an
/// event-specific upsert so call, recording, and transcript data can arrive in
/// any order without overwriting each other.
pub async fn event_progress(State(pool): State<MySqlPool>, body: Bytes) -> BasicResponse {
    let event = match TelnyxEvent::from_form(body.as_ref()) {
        Ok(event) => event,
        Err(_) => {
            tracing::error!("Error parsing Telnyx event-progress payload");
            return BAD_REQUEST;
        }
    };

    let raw_payload = String::from_utf8_lossy(body.as_ref()).into_owned();

    if event.has_transcript() {
        return handle_conversation_ended(&pool, &event, raw_payload).await;
    }

    if event.is_recording_event() {
        return handle_recording(&pool, &event).await;
    }

    if event.is_call_event() {
        return handle_call_completed(&pool, &event).await;
    }

    tracing::debug!(
        call_status = ?event.call_status,
        recording_status = ?event.recording_status,
        "Ignoring unrecognized Telnyx event"
    );
    OK_RESPONSE
}

async fn handle_call_completed(pool: &MySqlPool, event: &TelnyxEvent) -> BasicResponse {
    if event.call_sid.is_none() {
        tracing::warn!("Telnyx call completed event missing CallSid");
        return OK_RESPONSE;
    }

    match crate::crud::telnyx::upsert_call(pool, event).await {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!(?error, "Error upserting Telnyx call");
            internal_error(ERR_DB)
        }
    }
}

async fn handle_recording(pool: &MySqlPool, event: &TelnyxEvent) -> BasicResponse {
    if event.call_sid.is_none() {
        tracing::warn!("Telnyx recording event missing CallSid");
        return OK_RESPONSE;
    }

    match crate::crud::telnyx::upsert_recording(pool, event).await {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!(?error, "Error upserting Telnyx recording");
            internal_error(ERR_DB)
        }
    }
}

async fn handle_conversation_ended(
    pool: &MySqlPool,
    event: &TelnyxEvent,
    raw_payload: String,
) -> BasicResponse {
    if event.call_sid.is_none() {
        tracing::warn!(
            conversation_id = ?event.conversation_id,
            "Telnyx conversation_ended event missing CallSid"
        );
        return OK_RESPONSE;
    }

    let (messages_json, transcript) = match event.parse_messages() {
        Some(messages) => {
            let transcript = transcript_text(&messages);
            let messages_json = serde_json::to_string(&messages).unwrap_or_default();
            (Some(messages_json), Some(transcript))
        }
        None => match fetch_transcript(event).await {
            Some(messages) => {
                let transcript = api_messages_to_text(&messages);
                let messages_json = serde_json::to_string(&messages).unwrap_or_default();
                (Some(messages_json), Some(transcript))
            }
            None => {
                tracing::warn!(
                    conversation_id = ?event.conversation_id,
                    "Telnyx conversation_ended event had no embedded or API transcript"
                );
                (None, None)
            }
        },
    };

    match crate::crud::telnyx::upsert_conversation(
        pool,
        event,
        messages_json,
        transcript,
        raw_payload,
    )
    .await
    {
        Ok(_) => OK_RESPONSE,
        Err(error) => {
            tracing::error!(?error, "Error upserting Telnyx conversation");
            internal_error(ERR_DB)
        }
    }
}

async fn fetch_transcript(event: &TelnyxEvent) -> Option<Vec<ConversationMessage>> {
    let conversation_id = event.conversation_id.as_deref()?;
    match fetch_conversation_messages(conversation_id).await {
        Ok(messages) if !messages.is_empty() => Some(messages),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(?error, conversation_id, "Failed to fetch Telnyx transcript");
            None
        }
    }
}
