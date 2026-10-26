//! `OpenAI` chat completions with usage logging (`ai_usage_log`), like the
//! CRM's `trackedChatCompletion`. The key is `OPEN_AI_SECRET_KEY`.

use lambda_http::tracing;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CHAT_URL: &str = "https://api.openai.com/v1/chat/completions";
const IMAGE_EDITS_URL: &str = "https://api.openai.com/v1/images/edits";
/// The CRM's main chat model (`GPT_MINI_MODEL`).
pub const MINI_MODEL: &str = "gpt-6-luna";
/// When the key may not use [`MINI_MODEL`] (`GPT_MINI_FALLBACK_MODEL`).
pub const MINI_FALLBACK_MODEL: &str = "gpt-5.4-mini-2026-03-17";
/// The larger, slower model for writing that must sound like the rep and
/// use the customer's details (the AI Reply button; the follow-up writer's
/// `AI_FOLLOWUP_MODEL`). It rejects `reasoning_effort: none`.
pub const LARGE_MODEL: &str = "gpt-6.1-sol";
/// AI Design's kitchen pictures (`OPENAI_IMAGE_MODEL`).
pub const IMAGE_MODEL: &str = "gpt-image-2.5-sunburst";

#[derive(Clone)]
pub struct Ai {
    key: Option<Arc<str>>,
    url: Arc<str>,
    images_url: Arc<str>,
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

/// Sending an OpenAI request, once more when the connection failed before any answer.
///
/// The Lambda is frozen between invocations; a pooled connection the server
/// closed meanwhile fails the next request at once ("error sending request").
/// A request that timed out is not sent again.
trait SendRetrying {
    async fn send_retrying(self) -> Result<reqwest::Response, AiError>;
}

impl SendRetrying for reqwest::RequestBuilder {
    async fn send_retrying(self) -> Result<reqwest::Response, AiError> {
        let again = self.try_clone();
        match self.send().await {
            Ok(response) => Ok(response),
            Err(err) if err.is_timeout() => Err(request_failed(err)),
            Err(err) => {
                let error = request_failed_text(&err);
                tracing::warn!(%error, "OpenAI request failed; sending it once more");
                match again {
                    Some(request) => request.send().await.map_err(request_failed),
                    None => Err(request_failed(err)),
                }
            }
        }
    }
}

/// reqwest's error with its causes, without the URL: "error sending request:
/// connection reset by peer", "timed out", "error decoding response body: ...".
fn request_failed_text(err: &reqwest::Error) -> String {
    let mut text = if err.is_timeout() {
        "timed out"
    } else if err.is_decode() {
        "error decoding response body"
    } else if err.is_body() {
        "error reading response body"
    } else {
        "error sending request"
    }
    .to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

fn request_failed(err: reqwest::Error) -> AiError {
    AiError::Request(request_failed_text(&err))
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

    /// A key is set: calls can be made.
    pub fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    /// `OPEN_AI_SECRET_KEY`; `OPENAI_CHAT_URL` and `OPENAI_IMAGES_URL` point
    /// at a stand-in (the frontend's browser tests).
    pub fn from_env() -> Self {
        let env = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|url| !url.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self::new(
            std::env::var("OPEN_AI_SECRET_KEY").ok().as_deref(),
            &env("OPENAI_CHAT_URL", CHAT_URL),
        )
        .with_images_url(&env("OPENAI_IMAGES_URL", IMAGE_EDITS_URL))
    }

    /// The image edits endpoint (a stub in tests).
    #[must_use]
    pub fn with_images_url(mut self, url: &str) -> Self {
        self.images_url = Arc::from(url);
        self
    }

    /// `url` is the chat completions endpoint (a stub in tests).
    pub fn new(key: Option<&str>, url: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            url: Arc::from(url),
            images_url: Arc::from(IMAGE_EDITS_URL),
            http: common::http::client(),
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
                .send_retrying()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            let reply: Reply = response.json().await.map_err(request_failed)?;
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
                .send_retrying()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            let reply: Value = response.json().await.map_err(request_failed)?;
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
                .send_retrying()
                .await?;
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
            while let Some(chunk) = response.chunk().await.map_err(request_failed)? {
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
        let error = result.as_ref().err().map(ToString::to_string);
        let usage = if error.is_some() { None } else { usage };
        log(pool, caller, chat.model, usage, error.as_deref(), elapsed).await;
        result
    }
}

/// A picture for [`Ai::image_edit`].
pub struct EditImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What an image edit cost, for the log.
#[derive(Debug, Clone, Copy, Default)]
struct ImageUsage {
    input: Option<u64>,
    output: Option<u64>,
    image_input: Option<u64>,
    image_output: Option<u64>,
    images: u64,
}

impl Ai {
    /// One edited picture from `images` and `prompt` (`trackedImageEdit`):
    /// a `data:` URL, or `None` when the model returned none. Logged as an
    /// `image` call.
    pub async fn image_edit(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        model: &str,
        prompt: &str,
        images: &[EditImage],
        size: &str,
    ) -> Result<Option<String>, AiError> {
        let key = self.key.as_ref().ok_or(AiError::NotConfigured)?;
        let started = Instant::now();
        let boundary = format!("----granite{}", uuid::Uuid::new_v4().simple());
        let mut body: Vec<u8> = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
            );
        };
        field("model", model);
        field("prompt", prompt);
        field("size", size);
        field("quality", "high");
        for (index, image) in images.iter().enumerate() {
            let extension = if image.mime.contains("jpeg") || image.mime.contains("jpg") {
                "jpg"
            } else if image.mime.contains("webp") {
                "webp"
            } else {
                "png"
            };
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"image[]\"; filename=\"image-{index}.{extension}\"\r\nContent-Type: {}\r\n\r\n",
                    image.mime
                )
                .as_bytes(),
            );
            body.extend_from_slice(&image.bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let result = async {
            let response = self
                .http
                .post(&*self.images_url)
                .bearer_auth(&**key)
                .timeout(Duration::from_secs(600))
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body)
                .send_retrying()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(AiError::Status {
                    status: status.as_u16(),
                    body: text.chars().take(300).collect(),
                });
            }
            let reply: Value = response.json().await.map_err(request_failed)?;
            let image = reply["data"][0]["b64_json"].as_str().map(|b64| {
                format!(
                    "data:image/{};base64,{b64}",
                    reply["output_format"].as_str().unwrap_or("png")
                )
            });
            let u = &reply["usage"];
            let usage = ImageUsage {
                input: u["input_tokens"].as_u64(),
                output: u["output_tokens"].as_u64(),
                image_input: u["input_tokens_details"]["image_tokens"].as_u64(),
                image_output: u["output_tokens_details"]["image_tokens"]
                    .as_u64()
                    .or_else(|| u["output_tokens"].as_u64()),
                images: u64::from(image.is_some()),
            };
            Ok((image, usage))
        }
        .await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match result {
            Ok((image, usage)) => {
                log_image(pool, caller, model, Some(usage), None, elapsed).await;
                Ok(image)
            }
            Err(err) => {
                log_image(pool, caller, model, None, Some(&err.to_string()), elapsed).await;
                Err(err)
            }
        }
    }
}

/// An `image` row of `ai_usage_log`, priced like the CRM's `computeCostUsd`.
#[allow(clippy::suboptimal_flops)]
async fn log_image(
    pool: &MySqlPool,
    caller: Caller<'_>,
    model: &str,
    usage: Option<ImageUsage>,
    error: Option<&str>,
    duration_ms: u32,
) {
    let result = async {
        type Pricing = (Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);
        let pricing: Option<Pricing> = match usage {
            Some(_) => sqlx::query_as(
                "SELECT CAST(input_per_1m_usd AS DOUBLE), CAST(output_per_1m_usd AS DOUBLE),
                        CAST(image_input_per_1m_usd AS DOUBLE), CAST(image_output_per_1m_usd AS DOUBLE),
                        CAST(per_image_usd AS DOUBLE)
                   FROM ai_model_pricing WHERE model = ? AND effective_from <= CURDATE()
                  ORDER BY effective_from DESC LIMIT 1",
            )
            .bind(model)
            .fetch_optional(pool)
            .await?,
            None => None,
        };
        #[allow(clippy::cast_precision_loss)]
        let cost = match (usage, pricing) {
            (Some(u), Some((input, output, image_input, image_output, per_image))) => {
                let total_in = u.input.unwrap_or(0) as f64;
                let total_out = u.output.unwrap_or(0) as f64;
                if total_in > 0.0 || total_out > 0.0 {
                    let image_in = (u.image_input.unwrap_or(0) as f64).min(total_in);
                    let image_out = (u.image_output.unwrap_or(0) as f64).min(total_out);
                    ((total_in - image_in) * input.unwrap_or(0.0)
                        + image_in * image_input.unwrap_or(0.0)
                        + (total_out - image_out) * output.unwrap_or(0.0)
                        + image_out * image_output.unwrap_or(0.0))
                        / 1_000_000.0
                } else {
                    u.images as f64 * per_image.unwrap_or(0.0)
                }
            }
            _ => 0.0,
        };
        sqlx::query(
            "INSERT INTO ai_usage_log (feature, api_kind, model, user_id, company_id, input_tokens, output_tokens,
                image_input_tokens, image_output_tokens, image_count, cost_usd, success, error_message, duration_ms)
             VALUES (?, 'image', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(caller.feature)
        .bind(model)
        .bind(caller.user_id)
        .bind(caller.company_id)
        .bind(usage.and_then(|u| u.input))
        .bind(usage.and_then(|u| u.output))
        .bind(usage.and_then(|u| u.image_input))
        .bind(usage.and_then(|u| u.image_output))
        .bind(usage.map(|u| u.images))
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
        tracing::warn!(error = %err, feature = caller.feature, "AI image usage not logged");
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
