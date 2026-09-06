use serde::{Deserialize, Serialize};

/// A Telnyx call-progress webhook delivered as `application/x-www-form-urlencoded`.
///
/// Telnyx sends several different events to the same `event-progress` URL:
/// - `CallStatus=completed` — the normal call leg ended
/// - `CallStatus=conversation_ended` — the AI assistant conversation ended and a
///   transcript is available (in `Messages`)
/// - `RecordingStatus=completed` — a recording was saved (no transcript)
///
/// Unknown fields are ignored so the schema can keep receiving new event variants
/// without a deploy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TelnyxEvent {
    // Identity and routing
    pub account_sid: Option<String>,
    pub call_session_id: Option<String>,
    pub call_sid: Option<String>,
    pub call_sid_legacy: Option<String>,
    pub call_control_id: Option<String>,
    pub call_leg_id: Option<String>,
    pub connection_id: Option<String>,
    pub callback_source: Option<String>,

    // Event status
    pub call_status: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub caller_id: Option<String>,
    pub timestamp: Option<String>,
    pub occurred_at: Option<String>,
    pub sequence_number: Option<i64>,

    // Call completion fields
    pub hangup_cause: Option<String>,
    pub hangup_source: Option<String>,
    pub sip_hangup_cause: Option<String>,
    pub call_duration: Option<i64>,
    pub dial_call_duration: Option<i64>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub answered_time: Option<String>,
    pub call_initiated_at: Option<String>,

    // Recording fields
    pub recording_sid: Option<String>,
    pub recording_status: Option<String>,
    pub recording_source: Option<String>,
    pub recording_channels: Option<i32>,
    pub recording_duration: Option<i64>,
    pub recording_start_time: Option<String>,
    pub recording_end_time: Option<String>,
    pub recording_url: Option<String>,

    // AI assistant conversation fields
    pub conversation_id: Option<String>,
    pub assistant_id: Option<String>,
    pub duration_sec: Option<i64>,
    pub llm_model: Option<String>,
    pub stt_model: Option<String>,
    pub tts_model_id: Option<String>,
    pub tts_provider: Option<String>,
    pub tts_voice_id: Option<String>,
    pub reason: Option<String>,
    /// URL-encoded JSON string holding the conversation history.
    pub messages: Option<String>,
    /// Some TeXML variants base64-encode the same conversation history.
    pub base64_message_history: Option<String>,
}

impl TelnyxEvent {
    pub fn from_form(body: &[u8]) -> Result<Self, serde_urlencoded::de::Error> {
        serde_urlencoded::from_bytes(body)
    }

    /// `conversation_ended` is the event that tells us a transcript is ready.
    pub fn has_transcript(&self) -> bool {
        self.call_status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("conversation_ended"))
    }

    /// `RecordingStatus=completed` is a final recording-saved event.
    pub fn is_recording_event(&self) -> bool {
        self.recording_status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("completed"))
    }

    /// `CallStatus=completed` is the final call-progress event.
    pub fn is_call_event(&self) -> bool {
        self.call_status
            .as_deref()
            .is_some_and(|status| status.eq_ignore_ascii_case("completed"))
    }

    /// Parse `Messages` (JSON) or fall back to a base64-encoded message history.
    pub fn parse_messages(&self) -> Option<Vec<TelnyxMessage>> {
        let raw = self
            .messages
            .as_deref()
            .or(self.base64_message_history.as_deref())?;

        if let Ok(messages) = serde_json::from_str::<Vec<TelnyxMessage>>(raw) {
            return Some(messages);
        }

        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let decoded = STANDARD.decode(raw).ok()?;
        serde_json::from_slice::<Vec<TelnyxMessage>>(&decoded).ok()
    }
}

/// A single conversation turn as delivered inside the webhook `Messages` field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelnyxMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
}

impl TelnyxMessage {
    pub fn body(&self) -> String {
        self.content
            .clone()
            .or_else(|| self.text.clone())
            .unwrap_or_default()
    }
}

/// Flatten the conversation history into a single readable transcript.
pub fn transcript_text(messages: &[TelnyxMessage]) -> String {
    messages
        .iter()
        .filter_map(|message| {
            let body = message.body();
            if body.is_empty() {
                None
            } else {
                Some(format!("{}: {}", message.role, body))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{TelnyxEvent, transcript_text};

    #[test]
    fn parses_conversation_ended_event() {
        let body = b"AccountSid=447ef313-3e35-4a7f-8097-efa9f5a0e029\
&CallSessionId=d61e8124-aa09-11f1-99b4-3af7875816e4\
&CallSid=v3%3A4VV3YrEdL21GSwAjaCKKnkJxXH4xophWjjwofW_QBelFdsUmTcS8xA\
&ConnectionId=3038042337637828582\
&CallStatus=conversation_ended\
&ConversationId=fa21b464-47c4-4b21-bb7e-f39bb833c91c\
&DurationSec=17\
&LlmModel=openai%2Fgpt-5.6-luna\
&SttModel=deepgram%2Fflux\
&TtsModelId=Ultra\
&TtsProvider=telnyx\
&Reason=hangup\
&Messages=%5B%7B%22role%22%3A%22assistant%22%2C%22content%22%3A%22Hi%2C%20how%20can%20I%20help%20you%20today%3F%22%7D%2C%7B%22role%22%3A%22user%22%2C%22content%22%3A%22I%27d%20like%20to%20learn%20more%20about%20countertops.%22%7D%5D";

        let event = TelnyxEvent::from_form(body).expect("form should parse");
        assert!(event.has_transcript());
        assert_eq!(
            event.conversation_id.as_deref(),
            Some("fa21b464-47c4-4b21-bb7e-f39bb833c91c")
        );
        assert_eq!(event.duration_sec, Some(17));
        assert_eq!(event.llm_model.as_deref(), Some("openai/gpt-5.6-luna"));

        let messages = event.parse_messages().expect("messages should parse");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "assistant");
        assert_eq!(
            messages[1].body(),
            "I'd like to learn more about countertops."
        );

        let text = transcript_text(&messages);
        assert!(text.contains("assistant: Hi, how can I help you today?"));
        assert!(text.contains("user: I'd like to learn more about countertops."));
    }

    #[test]
    fn ignores_non_transcript_events() {
        let recording =
            b"RecordingStatus=completed&RecordingSid=0b88977a-346b-4ed7-b379-d41af9e47e63";
        let event = TelnyxEvent::from_form(recording).expect("form should parse");
        assert!(!event.has_transcript());
        assert!(event.is_recording_event());
        assert!(!event.is_call_event());
        assert_eq!(event.call_status, None);
    }

    #[test]
    fn classifies_call_completed_event() {
        let body = b"CallStatus=completed&CallSid=v3%3Aabc&CallDuration=17";
        let event = TelnyxEvent::from_form(body).expect("form should parse");
        assert!(event.is_call_event());
        assert!(!event.has_transcript());
        assert!(!event.is_recording_event());
        assert_eq!(event.call_sid.as_deref(), Some("v3:abc"));
        assert_eq!(event.call_duration, Some(17));
    }

    #[test]
    fn parses_base64_message_history() {
        let history = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            r#"[{"role":"user","content":"Hello"}]"#,
        );
        let body = format!(
            "CallStatus=conversation_ended&ConversationId=abc&Base64MessageHistory={history}"
        );
        let event = TelnyxEvent::from_form(body.as_bytes()).expect("form should parse");
        let messages = event
            .parse_messages()
            .expect("base64 messages should parse");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body(), "Hello");
    }
}
