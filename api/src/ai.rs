//! `OpenAI` chat completions with usage logging (`ai_usage_log`), like the
//! CRM's `trackedChatCompletion`. The key is `OPEN_AI_SECRET_KEY`.

use lambda_http::tracing;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHAT_URL: &str = "https://api.openai.com/v1/chat/completions";
/// The CRM's main chat model (`GPT_MINI_MODEL`).
pub const MINI_MODEL: &str = "gpt-6-luna";

#[derive(Clone)]
pub struct Ai {
    key: Option<Arc<str>>,
    url: Arc<str>,
    http: reqwest::Client,
}

/// Who a call is billed to, by feature.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    pub feature: &'a str,
    pub user_id: Option<i32>,
    pub company_id: Option<i32>,
}

/// A chat request.
#[derive(Debug, Clone)]
pub struct Chat<'a> {
    pub model: &'a str,
    /// `{ "role": ..., "content": ... }` messages.
    pub messages: Vec<Value>,
    /// Ask for a JSON object reply.
    pub json: bool,
    pub reasoning_effort: Option<&'a str>,
    pub timeout: Duration,
}

impl<'a> Chat<'a> {
    pub const fn new(model: &'a str) -> Self {
        Self {
            model,
            messages: Vec::new(),
            json: false,
            reasoning_effort: None,
            timeout: Duration::from_secs(60),
        }
    }

    #[must_use]
    pub fn message(mut self, role: &str, content: &str) -> Self {
        self.messages
            .push(json!({ "role": role, "content": content }));
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("OPEN_AI_SECRET_KEY is not set")]
    NotConfigured,
    #[error("OpenAI request failed: {0}")]
    Request(String),
    #[error("OpenAI returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("OpenAI returned no content")]
    Empty,
}

#[derive(Deserialize)]
struct Reply {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
}

#[derive(Deserialize, Clone, Copy, Default)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<CachedTokens>,
}

#[derive(Deserialize, Clone, Copy, Default)]
struct CachedTokens {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

impl Ai {
    pub fn disabled() -> Self {
        Self::new(None, CHAT_URL)
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("OPEN_AI_SECRET_KEY").ok().as_deref(),
            CHAT_URL,
        )
    }

    /// `url` is the chat completions endpoint (a stub in tests).
    pub fn new(key: Option<&str>, url: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            url: Arc::from(url),
            http: reqwest::Client::new(),
        }
    }

    /// The reply text, logging tokens and cost (or the failure) for `caller`.
    pub async fn chat(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        chat: &Chat<'_>,
    ) -> Result<String, AiError> {
        let key = self.key.as_ref().ok_or(AiError::NotConfigured)?;
        let started = Instant::now();
        let mut body = json!({ "model": chat.model, "messages": chat.messages });
        if chat.json {
            body["response_format"] = json!({ "type": "json_object" });
        }
        if let Some(effort) = chat.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let result = async {
            let response = self
                .http
                .post(&*self.url)
                .bearer_auth(&**key)
                .timeout(chat.timeout)
                .json(&body)
                .send()
                .await
                .map_err(|err| AiError::Request(err.without_url().to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            let reply: Reply = response
                .json()
                .await
                .map_err(|err| AiError::Request(err.without_url().to_string()))?;
            let usage = reply.usage.unwrap_or_default();
            let text = reply
                .choices
                .into_iter()
                .next()
                .and_then(|choice| choice.message.content)
                .unwrap_or_default();
            Ok((text, usage))
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match result {
            Ok((text, usage)) => {
                log(pool, caller, chat.model, Some(usage), None, elapsed).await;
                Ok(text)
            }
            Err(err) => {
                let message = err.to_string();
                log(pool, caller, chat.model, None, Some(&message), elapsed).await;
                Err(err)
            }
        }
    }
}

/// One `ai_usage_log` row; a failure to log is only traced.
async fn log(
    pool: &MySqlPool,
    caller: Caller<'_>,
    model: &str,
    usage: Option<Usage>,
    error: Option<&str>,
    duration_ms: u32,
) {
    let result = async {
        let pricing: Option<(Option<f64>, Option<f64>, Option<f64>)> = match usage {
            Some(_) => sqlx::query_as(
                "SELECT CAST(input_per_1m_usd AS DOUBLE), CAST(cached_input_per_1m_usd AS DOUBLE),
                            CAST(output_per_1m_usd AS DOUBLE)
                       FROM ai_model_pricing WHERE model = ? AND effective_from <= CURDATE()
                      ORDER BY effective_from DESC LIMIT 1",
            )
            .bind(model)
            .fetch_optional(pool)
            .await?,
            None => None,
        };
        let cost = match (usage, pricing) {
            (Some(usage), Some((input, cached, output))) => {
                let total_in = f64::from(usage.prompt_tokens);
                let cached_in = f64::from(
                    usage
                        .prompt_tokens_details
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0),
                )
                .min(total_in);
                let out = f64::from(usage.completion_tokens);
                out.mul_add(
                    output.unwrap_or(0.0),
                    cached_in.mul_add(
                        cached.unwrap_or(0.0),
                        (total_in - cached_in) * input.unwrap_or(0.0),
                    ),
                ) / 1_000_000.0
            }
            _ => 0.0,
        };
        sqlx::query(
            "INSERT INTO ai_usage_log (feature, api_kind, model, user_id, company_id, input_tokens,
                cached_input_tokens, output_tokens, cost_usd, success, error_message, duration_ms)
             VALUES (?, 'chat', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(caller.feature)
        .bind(model)
        .bind(caller.user_id)
        .bind(caller.company_id)
        .bind(usage.map(|u| u.prompt_tokens))
        .bind(usage.map(|u| {
            u.prompt_tokens_details
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0)
        }))
        .bind(usage.map(|u| u.completion_tokens))
        .bind(cost)
        .bind(error.is_none())
        .bind(error.map(|e| e.chars().take(500).collect::<String>()))
        .bind(duration_ms)
        .execute(pool)
        .await?;
        Ok::<_, sqlx::Error>(())
    }
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, feature = caller.feature, "AI usage not logged");
    }
}
