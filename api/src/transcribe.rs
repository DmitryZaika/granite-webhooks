//! Speech to text with `AssemblyAI`, logged to `ai_usage_log` like the CRM's
//! `trackedTranscription`. The key is `ASSEMBLYAI_API_KEY`.

use crate::ai::Caller;
use lambda_http::tracing;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BASE_URL: &str = "https://api.assemblyai.com";
/// The CRM's `ASSEMBLYAI_TRANSCRIBE_MODEL`.
pub const MODEL: &str = "universal-3-5-pro";
const MAX_POLLS: u32 = 200;

#[derive(Clone)]
pub struct Transcriber {
    key: Option<Arc<str>>,
    base: Arc<str>,
    poll_every: Duration,
    http: reqwest::Client,
}

/// One speaker's turn.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct Utterance {
    /// `A`, `B`, ...
    pub speaker: String,
    /// What they said.
    pub text: String,
}

/// A finished transcript.
#[derive(Debug, Clone)]
pub struct Transcript {
    pub text: String,
    /// Turns in order; only when speaker labels were asked for.
    pub utterances: Option<Vec<Utterance>>,
    pub audio_seconds: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    #[error("ASSEMBLYAI_API_KEY is not set")]
    NotConfigured,
    #[error("AssemblyAI request failed: {0}")]
    Request(String),
    #[error("AssemblyAI transcription failed: {0}")]
    Failed(String),
    #[error("AssemblyAI transcription timed out")]
    TimedOut,
}

impl Transcriber {
    pub fn disabled() -> Self {
        Self::new(None, BASE_URL)
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("ASSEMBLYAI_API_KEY").ok().as_deref(),
            BASE_URL,
        )
    }

    /// `base` is the API root (a stub in tests, which also poll quickly).
    pub fn new(key: Option<&str>, base: &str) -> Self {
        let poll_every = if base == BASE_URL {
            Duration::from_secs(3)
        } else {
            Duration::from_millis(10)
        };
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            base: Arc::from(base.trim_end_matches('/')),
            poll_every,
            http: reqwest::Client::new(),
        }
    }

    /// Uploads `audio`, waits for the transcript and logs the audio minutes
    /// (or the failure) for `caller`.
    pub async fn transcribe(
        &self,
        pool: &MySqlPool,
        caller: Caller<'_>,
        audio: Vec<u8>,
        speaker_labels: bool,
    ) -> Result<Transcript, TranscribeError> {
        let started = Instant::now();
        let result = self.run(audio, speaker_labels).await;
        let elapsed = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (seconds, error) = match &result {
            Ok(transcript) => (transcript.audio_seconds, None),
            Err(err) => (None, Some(err.to_string())),
        };
        log(pool, caller, seconds, error.as_deref(), elapsed).await;
        result
    }

    async fn json(
        &self,
        request: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<Value, TranscribeError> {
        let response = request
            .send()
            .await
            .map_err(|err| TranscribeError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(TranscribeError::Request(format!(
                "{what} failed ({})",
                response.status().as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|err| TranscribeError::Request(err.without_url().to_string()))
    }

    async fn run(
        &self,
        audio: Vec<u8>,
        speaker_labels: bool,
    ) -> Result<Transcript, TranscribeError> {
        let key = self.key.as_deref().ok_or(TranscribeError::NotConfigured)?;
        let uploaded = self
            .json(
                self.http
                    .post(format!("{}/v2/upload", self.base))
                    .header("authorization", key)
                    .body(audio),
                "upload",
            )
            .await?;
        let upload_url = uploaded["upload_url"]
            .as_str()
            .ok_or_else(|| TranscribeError::Request("upload did not return upload_url".into()))?;
        let mut request = json!({
            "audio_url": upload_url,
            "speech_models": [MODEL],
            "language_code": "en",
        });
        if speaker_labels {
            request["speaker_labels"] = json!(true);
        }
        let created = self
            .json(
                self.http
                    .post(format!("{}/v2/transcript", self.base))
                    .header("authorization", key)
                    .json(&request),
                "transcript request",
            )
            .await?;
        let id = created["id"].as_str().ok_or_else(|| {
            TranscribeError::Request("transcript request did not return an id".into())
        })?;
        for _ in 0..MAX_POLLS {
            let transcript = self
                .json(
                    self.http
                        .get(format!("{}/v2/transcript/{id}", self.base))
                        .header("authorization", key),
                    "transcript poll",
                )
                .await?;
            match transcript["status"].as_str() {
                Some("completed") => return Ok(finished(&transcript, speaker_labels)),
                Some("error") => {
                    return Err(TranscribeError::Failed(
                        transcript["error"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string(),
                    ));
                }
                _ => tokio::time::sleep(self.poll_every).await,
            }
        }
        Err(TranscribeError::TimedOut)
    }
}

fn finished(transcript: &Value, speaker_labels: bool) -> Transcript {
    let utterances = speaker_labels.then(|| {
        transcript["utterances"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|turn| {
                Some(Utterance {
                    speaker: turn["speaker"].as_str()?.to_string(),
                    text: turn["text"].as_str()?.to_string(),
                })
            })
            .collect()
    });
    Transcript {
        text: transcript["text"].as_str().unwrap_or("").to_string(),
        utterances,
        audio_seconds: transcript["audio_duration"].as_f64(),
    }
}

/// One `ai_usage_log` row; a failure to log is only traced.
async fn log(
    pool: &MySqlPool,
    caller: Caller<'_>,
    audio_seconds: Option<f64>,
    error: Option<&str>,
    duration_ms: u32,
) {
    let result = async {
        let per_minute: Option<Option<f64>> = if error.is_none() {
            sqlx::query_scalar(
                "SELECT CAST(per_minute_usd AS DOUBLE) FROM ai_model_pricing
                  WHERE model = ? AND effective_from <= CURDATE() ORDER BY effective_from DESC LIMIT 1",
            )
            .bind(MODEL)
            .fetch_optional(pool)
            .await?
        } else {
            None
        };
        let cost = audio_seconds.unwrap_or(0.0) / 60.0 * per_minute.flatten().unwrap_or(0.0);
        sqlx::query(
            "INSERT INTO ai_usage_log (feature, api_kind, model, user_id, company_id, audio_seconds,
                cost_usd, success, error_message, duration_ms)
             VALUES (?, 'transcription', ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(caller.feature)
        .bind(MODEL)
        .bind(caller.user_id)
        .bind(caller.company_id)
        .bind(audio_seconds)
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
        tracing::warn!(error = %err, feature = caller.feature, "transcription usage not logged");
    }
}
