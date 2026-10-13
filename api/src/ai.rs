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
/// When the key may not use [`MINI_MODEL`] (`GPT_MINI_FALLBACK_MODEL`).
pub const MINI_FALLBACK_MODEL: &str = "gpt-5.4-mini-2026-03-17";
/// AI Design's kitchen pictures (`OPENAI_IMAGE_MODEL`).
pub const IMAGE_MODEL: &str = "gpt-image-2.5-sunburst";

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
    pub max_completion_tokens: Option<u32>,
    /// Routes calls with the same prompt prefix to the same cache.
    pub prompt_cache_key: Option<String>,
    pub timeout: Duration,
}

impl<'a> Chat<'a> {
    pub const fn new(model: &'a str) -> Self {
        Self {
            model,
            messages: Vec::new(),
            json: false,
            reasoning_effort: None,
            max_completion_tokens: None,
            prompt_cache_key: None,
            timeout: Duration::from_secs(60),
        }
    }

    #[must_use]
    pub const fn max_tokens(mut self, max: Option<u32>) -> Self {
        self.max_completion_tokens = max;
        self
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

impl AiError {
    /// The key may not use the model (the caller falls back to another).
    pub fn is_model_permission(&self) -> bool {
        let Self::Status { status, body } = self else {
            return false;
        };
        let body = body.to_lowercase();
        body.contains("insufficient permissions")
            || body.contains("missing scopes")
            || body.contains("does not have access to model")
            || body.contains("model_not_found")
            || (*status == 401 && body.contains("permission"))
            || (*status == 403 && body.contains("model"))
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    model: Option<String>,
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// A reply with what the pipeline metrics record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    pub text: String,
    /// The model that answered (the requested one when not reported).
    pub model: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub finish_reason: Option<String>,
    pub duration_ms: u32,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
}

#[derive(Deserialize, Clone, Copy, Default, Debug)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<CachedTokens>,
}

#[derive(Deserialize, Clone, Copy, Default, Debug)]
struct CachedTokens {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

impl Ai {
    pub fn disabled() -> Self {
        Self::new(None, CHAT_URL)
    }

    /// `OPEN_AI_SECRET_KEY`; `OPENAI_CHAT_URL` points at a stand-in (the
    /// frontend's browser tests).
    pub fn from_env() -> Self {
        let url = std::env::var("OPENAI_CHAT_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| CHAT_URL.to_string());
        Self::new(std::env::var("OPEN_AI_SECRET_KEY").ok().as_deref(), &url)
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
        self.chat_reply(pool, caller, chat)
            .await
            .map(|reply| reply.text)
    }

    /// Like [`Ai::chat`], with the model, tokens, finish reason and time.
    pub async fn chat_reply(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        chat: &Chat<'_>,
    ) -> Result<ChatReply, AiError> {
        let key = self.key.as_ref().ok_or(AiError::NotConfigured)?;
        let started = Instant::now();
        let mut body = json!({ "model": chat.model, "messages": chat.messages });
        if chat.json {
            body["response_format"] = json!({ "type": "json_object" });
        }
        if let Some(max) = chat.max_completion_tokens {
            body["max_completion_tokens"] = json!(max);
        }
        if let Some(effort) = chat.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(key) = &chat.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
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
            let usage = reply.usage;
            let first = reply.choices.into_iter().next();
            let finish_reason = first
                .as_ref()
                .and_then(|choice| choice.finish_reason.clone());
            let text = first
                .and_then(|choice| choice.message.content)
                .unwrap_or_default();
            Ok((text, usage, reply.model, finish_reason))
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match result {
            Ok((text, usage, model, finish_reason)) => {
                log(
                    pool,
                    caller,
                    chat.model,
                    Some(usage.unwrap_or_default()),
                    None,
                    elapsed,
                )
                .await;
                Ok(ChatReply {
                    text,
                    model: model.unwrap_or_else(|| chat.model.to_string()),
                    input_tokens: usage.map(|u| u.prompt_tokens),
                    output_tokens: usage.map(|u| u.completion_tokens),
                    finish_reason,
                    duration_ms: elapsed,
                })
            }
            Err(err) => {
                let message = err.to_string();
                log(pool, caller, chat.model, None, Some(&message), elapsed).await;
                Err(err)
            }
        }
    }
}

impl Ai {
    /// One completion with a request body built by the caller (tools,
    /// content parts): the first choice's `message`, usage logged like
    /// [`Ai::chat`]. `model` is the body's model, for the log.
    pub async fn complete(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        model: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<Value, AiError> {
        let key = self.key.as_ref().ok_or(AiError::NotConfigured)?;
        let started = Instant::now();
        let result = async {
            let response = self
                .http
                .post(&*self.url)
                .bearer_auth(&**key)
                .timeout(timeout)
                .json(body)
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
            let reply: Value = response
                .json()
                .await
                .map_err(|err| AiError::Request(err.without_url().to_string()))?;
            let usage: Option<Usage> = reply
                .get("usage")
                .cloned()
                .and_then(|u| serde_json::from_value(u).ok());
            Ok((reply["choices"][0]["message"].clone(), usage))
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match result {
            Ok((message, usage)) => {
                log(
                    pool,
                    caller,
                    model,
                    Some(usage.unwrap_or_default()),
                    None,
                    elapsed,
                )
                .await;
                Ok(message)
            }
            Err(err) => {
                log(pool, caller, model, None, Some(&err.to_string()), elapsed).await;
                Err(err)
            }
        }
    }

    /// A streamed completion: `on_text` gets each piece of the answer as it
    /// arrives; the whole answer is returned. Usage is logged once the
    /// stream ends (`trackedChatStream`).
    pub async fn chat_stream(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        chat: &Chat<'_>,
        mut on_text: impl FnMut(&str) + Send,
    ) -> Result<String, AiError> {
        let key = self.key.as_ref().ok_or(AiError::NotConfigured)?;
        let started = Instant::now();
        let mut body = json!({
            "model": chat.model,
            "messages": chat.messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(max) = chat.max_completion_tokens {
            body["max_completion_tokens"] = json!(max);
        }
        if let Some(effort) = chat.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let mut usage: Option<Usage> = None;
        let result = async {
            let mut response = self
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
            let mut answer = String::new();
            let mut pending: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| AiError::Request(err.without_url().to_string()))?
            {
                pending.extend_from_slice(&chunk);
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line);
                    let Some(data) = line.trim_end().strip_prefix("data:") else {
                        continue;
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
                        usage = serde_json::from_value(u.clone()).ok();
                        continue;
                    }
                    if let Some(text) = event["choices"][0]["delta"]["content"].as_str()
                        && !text.is_empty()
                    {
                        answer.push_str(text);
                        on_text(text);
                    }
                }
            }
            Ok(answer)
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match &result {
            Ok(_) => log(pool, caller, chat.model, usage, None, elapsed).await,
            Err(err) => {
                log(
                    pool,
                    caller,
                    chat.model,
                    None,
                    Some(&err.to_string()),
                    elapsed,
                )
                .await
            }
        }
        result
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
