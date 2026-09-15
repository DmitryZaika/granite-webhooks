use serde::{Deserialize, Serialize};

const TELNYX_API_BASE: &str = "https://api.telnyx.com/v2";

#[derive(Debug, Clone, Deserialize)]
pub struct ConversationMessageList {
    pub data: Vec<ConversationMessage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConversationMessage {
    pub role: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub sent_at: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

impl ConversationMessage {
    pub fn body(&self) -> String {
        self.text.clone().unwrap_or_default()
    }
}

pub fn api_messages_to_text(messages: &[ConversationMessage]) -> String {
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

/// Fetch the finalized transcript for an AI conversation from the Telnyx API.
///
/// Requires the `TELNYX_API_KEY` environment variable. The webhook normally
/// includes `Messages` already, so this is only used as a fallback when a
/// `conversation_ended` event arrives without an embedded transcript.
pub async fn fetch_conversation_messages(
    conversation_id: &str,
) -> Result<Vec<ConversationMessage>, String> {
    let api_key = std::env::var("TELNYX_API_KEY")
        .map_err(|_| "TELNYX_API_KEY environment variable is not set".to_string())?;

    let url = format!("{TELNYX_API_BASE}/ai/conversations/{conversation_id}/messages");
    let response = reqwest::Client::new()
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await
        .map_err(|error| format!("Telnyx API request failed: {error}"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Telnyx API {status}: {}",
            body.chars().take(500).collect::<String>()
        ));
    }

    let envelope: ConversationMessageList = serde_json::from_str(&body)
        .map_err(|error| format!("Telnyx API returned invalid JSON: {error}"))?;
    Ok(envelope.data)
}
