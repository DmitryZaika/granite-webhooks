use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

const CHAT_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";
/// Same variable the CRM (`general_datebase`) reads its API key from.
pub const API_KEY_ENV: &str = "OPEN_AI_SECRET_KEY";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{API_KEY_ENV} is not set")]
    MissingApiKey,
    #[error("OpenAI request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("OpenAI returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("OpenAI refused: {0}")]
    Refusal(String),
    #[error("OpenAI returned no content")]
    EmptyContent,
    #[error("OpenAI returned invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub cached_input_tokens: u32,
    pub output_tokens: u32,
}

/// A chat completion whose reply must match `schema` (strict structured output).
pub struct StructuredChat<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub schema_name: &'a str,
    pub schema: Value,
    pub max_completion_tokens: u32,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct ResponseUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
}

impl From<ResponseUsage> for Usage {
    fn from(usage: ResponseUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            cached_input_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens)
                .unwrap_or(0),
            output_tokens: usage.completion_tokens,
        }
    }
}

pub fn api_key_from_env() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// Returns the parsed reply plus token usage. Usage is also returned when the
/// reply could not be parsed, so failed calls can still be billed correctly.
pub async fn structured_chat<T: DeserializeOwned>(
    client: &Client,
    api_key: &str,
    request: &StructuredChat<'_>,
) -> (Result<T, Error>, Option<Usage>) {
    let body = json!({
        "model": request.model,
        "max_completion_tokens": request.max_completion_tokens,
        "messages": [
            { "role": "system", "content": request.system },
            { "role": "user", "content": request.user },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": request.schema_name,
                "strict": true,
                "schema": request.schema,
            },
        },
    });

    let response = match client
        .post(CHAT_COMPLETIONS_URL)
        .bearer_auth(api_key)
        .timeout(REQUEST_TIMEOUT)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => return (Err(e.into()), None),
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return (
            Err(Error::Status {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            }),
            None,
        );
    }

    let parsed = match response.json::<ChatResponse>().await {
        Ok(parsed) => parsed,
        Err(e) => return (Err(e.into()), None),
    };
    let usage = parsed.usage.map(Usage::from);
    let Some(message) = parsed
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message)
    else {
        return (Err(Error::EmptyContent), usage);
    };
    if let Some(refusal) = message.refusal {
        return (Err(Error::Refusal(refusal)), usage);
    }
    let Some(content) = message.content.filter(|c| !c.trim().is_empty()) else {
        return (Err(Error::EmptyContent), usage);
    };
    (serde_json::from_str(&content).map_err(Error::from), usage)
}
