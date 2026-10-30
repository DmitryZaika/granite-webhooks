//! Claude (Anthropic's Messages API) with usage logging (`ai_usage_log`),
//! like [`crate::ai`]. The key is `ANTHROPIC_API_KEY`.

use crate::ai::{Caller, log_chat};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
/// A request Claude declines is answered by another model the API picks
/// (`fallbacks: "default"`).
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The 10DLC check's second reviewer.
pub const OPUS_MODEL: &str = "claude-opus-5-5";

#[derive(Clone)]
pub struct Claude {
    key: Option<Arc<str>>,
    url: Arc<str>,
    http: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("ANTHROPIC_API_KEY is not set")]
    NotConfigured,
    #[error("Claude request failed: {0}")]
    Request(String),
    #[error("Claude returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("Claude declined to answer")]
    Refused,
    #[error("Claude's answer was cut off at {0} tokens")]
    Truncated(u32),
    #[error("Claude returned no content")]
    Empty,
}

/// One request.
#[derive(Debug, Clone)]
pub struct Message<'a> {
    pub model: &'a str,
    pub system: &'a str,
    /// `{ "role": ..., "content": ... }` turns, a user's first.
    pub messages: &'a [Value],
    /// `low` to `max`: how hard it thinks.
    pub effort: &'a str,
    /// The JSON schema the answer follows.
    pub schema: Option<&'a Value>,
    pub max_tokens: u32,
    pub timeout: Duration,
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// An answer.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Its text blocks.
    pub text: String,
    /// All its blocks (thinking too): an assistant turn sent back as they
    /// came, since Claude rejects an edited history.
    pub content: Value,
}

#[derive(Deserialize, Clone, Copy, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
}

fn request_failed(err: &reqwest::Error) -> ClaudeError {
    ClaudeError::Request(common::http::error_text(err))
}

impl Claude {
    pub fn disabled() -> Self {
        Self::new(None, MESSAGES_URL)
    }

    /// A key is set: calls can be made.
    pub fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    /// `ANTHROPIC_API_KEY`; `ANTHROPIC_MESSAGES_URL` points at a stand-in.
    pub fn from_env() -> Self {
        let url = std::env::var("ANTHROPIC_MESSAGES_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| MESSAGES_URL.to_string());
        Self::new(std::env::var("ANTHROPIC_API_KEY").ok().as_deref(), &url)
    }

    /// `url` is the Messages endpoint (a stub in tests).
    pub fn new(key: Option<&str>, url: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            url: Arc::from(url),
            http: common::http::client(),
        }
    }

    /// The answer, logging tokens and cost (or the failure) for `caller`
    /// under the model that answered.
    pub async fn message(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        request: &Message<'_>,
    ) -> Result<Answer, ClaudeError> {
        let key = self.key.as_ref().ok_or(ClaudeError::NotConfigured)?;
        let started = Instant::now();
        let mut output_config = json!({ "effort": request.effort });
        if let Some(schema) = request.schema {
            output_config["format"] = json!({ "type": "json_schema", "schema": schema });
        }
        let body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "system": request.system,
            "messages": request.messages,
            "output_config": output_config,
            "fallbacks": "default",
        });
        let result = async {
            let response = common::http::send_retrying(
                self.http
                    .post(&*self.url)
                    .header("x-api-key", &**key)
                    .header("anthropic-version", API_VERSION)
                    .header("anthropic-beta", FALLBACK_BETA)
                    .timeout(request.timeout)
                    .json(&body),
            )
            .await
            .map_err(|err| request_failed(&err))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(ClaudeError::Status {
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            let reply: Reply = response
                .json()
                .await
                .map_err(|err| request_failed(&err))?;
            let usage = reply.usage.unwrap_or_default();
            let model = reply.model.unwrap_or_else(|| request.model.to_string());
            let text: String = reply
                .content
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            let answer = match reply.stop_reason.as_deref() {
                Some("refusal") => Err(ClaudeError::Refused),
                Some("max_tokens") => Err(ClaudeError::Truncated(request.max_tokens)),
                _ if text.trim().is_empty() => Err(ClaudeError::Empty),
                _ => Ok(Answer {
                    text,
                    content: Value::Array(reply.content),
                }),
            };
            Ok((model, usage, answer))
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match result {
            Ok((model, usage, answer)) => {
                // Billed whether or not the answer is usable.
                let read = usage.cache_read_input_tokens.unwrap_or(0);
                let input =
                    usage.input_tokens + usage.cache_creation_input_tokens.unwrap_or(0) + read;
                let error = answer.as_ref().err().map(ToString::to_string);
                log_chat(
                    pool,
                    caller,
                    &model,
                    Some((input, read, usage.output_tokens)),
                    error.as_deref(),
                    elapsed,
                )
                .await;
                answer
            }
            Err(err) => {
                log_chat(
                    pool,
                    caller,
                    request.model,
                    None,
                    Some(&err.to_string()),
                    elapsed,
                )
                .await;
                Err(err)
            }
        }
    }
}
