//! Calls to the `webhooks` Lambda for work it already owns.
//!
//! That is CloudTalk contact sync and sending mail through SES. Configured
//! with `WEBHOOKS_URL` (its function URL) and `WEBHOOKS_KEY` (its
//! `REMIX_KEY`); without them the calls are skipped.

use lambda_http::tracing;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct WebhooksClient {
    target: Option<Arc<(String, String)>>,
    http: reqwest::Client,
}

impl WebhooksClient {
    pub fn disabled() -> Self {
        Self::new(None, None)
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var("WEBHOOKS_URL").ok().as_deref(),
            std::env::var("WEBHOOKS_KEY").ok().as_deref(),
        )
    }

    pub fn new(url: Option<&str>, key: Option<&str>) -> Self {
        let target = match (url.map(str::trim), key.map(str::trim)) {
            (Some(url), Some(key)) if !url.is_empty() && !key.is_empty() => Some(Arc::new((
                url.trim_end_matches('/').to_string(),
                key.to_string(),
            ))),
            _ => None,
        };
        Self {
            target,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client builds"),
        }
    }

    /// Pushes a customer to the company's CloudTalk address book. Best
    /// effort: companies without CloudTalk, and failures, are only logged.
    pub async fn sync_cloudtalk_contact(&self, company_id: i32, customer_id: i32) {
        self.sync_contact("cloudtalk", company_id, customer_id)
            .await;
    }

    /// Pushes a customer to the company's RingCentral contacts (best effort).
    pub async fn sync_ringcentral_contact(&self, company_id: i32, customer_id: i32) {
        self.sync_contact("ringcentral", company_id, customer_id)
            .await;
    }

    /// Both telephony address books (best effort).
    pub async fn sync_contacts(&self, company_id: i32, customer_id: i32) {
        tokio::join!(
            self.sync_cloudtalk_contact(company_id, customer_id),
            self.sync_ringcentral_contact(company_id, customer_id)
        );
    }

    async fn sync_contact(&self, provider: &str, company_id: i32, customer_id: i32) {
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .post(format!("{url}/{provider}/sync/{company_id}/{customer_id}"))
            .header("Authorization", key)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::info!(status = %response.status(), provider, customer_id, "contact sync skipped");
            }
            Err(err) => {
                tracing::warn!(error = %err.without_url(), provider, customer_id, "contact sync failed");
            }
        }
    }

    /// Sends one email (`POST /email/send`); returns its `Message-ID`.
    pub async fn send_email(&self, email: &OutgoingEmail) -> Result<String, SendFailure> {
        let Some(target) = &self.target else {
            return Err(SendFailure::Failed(
                "email sending is not configured".to_string(),
            ));
        };
        let (url, key) = target.as_ref();
        let response = self
            .http
            .post(format!("{url}/email/send"))
            .header("Authorization", key)
            .timeout(Duration::from_secs(90))
            .json(email)
            .send()
            .await
            .map_err(|err| SendFailure::Failed(err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            let sent: SentEmail = response
                .json()
                .await
                .map_err(|err| SendFailure::Failed(err.without_url().to_string()))?;
            return Ok(sent.message_id);
        }
        let error = response
            .json::<ErrorReply>()
            .await
            .map(|reply| reply.error)
            .unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST && !error.is_empty() {
            return Err(SendFailure::Rejected(error));
        }
        Err(SendFailure::Failed(format!("{status} {error}")))
    }
}

/// Body of the `webhooks` `POST /email/send`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub from_name: Option<String>,
    pub from_address: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub signature: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub content_type: String,
    pub url: String,
}

#[derive(Deserialize)]
struct SentEmail {
    message_id: String,
}

#[derive(Deserialize)]
struct ErrorReply {
    error: String,
}

#[derive(Debug)]
pub enum SendFailure {
    /// The message was refused (bad address, unreadable attachment): the
    /// reason is for the user.
    Rejected(String),
    /// SES or the Lambda failed.
    Failed(String),
}
