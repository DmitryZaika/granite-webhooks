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
            http: common::http::client_builder()
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

    /// Removes a deleted customer from both telephony address books (best effort).
    pub async fn remove_contacts(&self, company_id: i32, customer_id: i32) {
        tokio::join!(
            self.contact_request(
                reqwest::Method::DELETE,
                "cloudtalk",
                company_id,
                customer_id
            ),
            self.contact_request(
                reqwest::Method::DELETE,
                "ringcentral",
                company_id,
                customer_id
            )
        );
    }

    async fn sync_contact(&self, provider: &str, company_id: i32, customer_id: i32) {
        self.contact_request(reqwest::Method::POST, provider, company_id, customer_id)
            .await;
    }

    async fn contact_request(
        &self,
        method: reqwest::Method,
        provider: &str,
        company_id: i32,
        customer_id: i32,
    ) {
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .request(
                method,
                format!("{url}/{provider}/sync/{company_id}/{customer_id}"),
            )
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

impl WebhooksClient {
    /// Hands a lead the marketing team added to the lead pipeline
    /// (`POST /v1/webhooks/new-lead-form/{company_id}`): it finds the saved
    /// customer, opens a deal and notifies the sales team. Best effort.
    pub async fn notify_new_lead(&self, company_id: i32, lead: &serde_json::Value) {
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .post(format!("{url}/v1/webhooks/new-lead-form/{company_id}"))
            .header("Authorization", key)
            .json(lead)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(status = %response.status(), company_id, "new lead webhook refused");
            }
            Err(err) => tracing::warn!(error = %err.without_url(), "new lead webhook failed"),
        }
    }

    /// Deletes the Telegram messages about a deleted lead
    /// (`DELETE /telegram/lead-messages/{company_id}/{customer_id}`). Best effort.
    pub async fn delete_lead_messages(&self, company_id: i32, customer_id: i32) {
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .delete(format!(
                "{url}/telegram/lead-messages/{company_id}/{customer_id}"
            ))
            .header("Authorization", key)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(status = %response.status(), customer_id, "lead message cleanup refused");
            }
            Err(err) => tracing::warn!(error = %err.without_url(), "lead message cleanup failed"),
        }
    }

    /// A CRM notification to the user's Telegram (`POST /telegram/crm-notify`).
    /// Best effort: failures are only logged.
    pub async fn notify_telegram_crm(&self, notify: &TelegramCrmNotify) {
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .post(format!("{url}/telegram/crm-notify"))
            .header("Authorization", key)
            .json(notify)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(status = %response.status(), "CRM telegram notify refused");
            }
            Err(err) => tracing::warn!(error = %err.without_url(), "CRM telegram notify failed"),
        }
    }

    /// A message to the user's notifications Telegram bot
    /// (`POST /telegram/notifications-notify`); false when it did not go out.
    pub async fn notify_telegram(&self, user_id: i32, message: &str) -> bool {
        let Some(target) = &self.target else {
            return false;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .post(format!("{url}/telegram/notifications-notify"))
            .header("Authorization", key)
            .json(&serde_json::json!({ "user_id": user_id, "message": message }))
            .send()
            .await;
        match result {
            Ok(response) => response.status().is_success(),
            Err(err) => {
                tracing::warn!(error = %err.without_url(), "telegram notify failed");
                false
            }
        }
    }

    /// Sends a text (`POST /sms/send`); the ids of the rows sent.
    pub async fn send_sms(&self, sms: &OutgoingSms) -> Result<Vec<i64>, SmsFailure> {
        let Some(target) = &self.target else {
            return Err(SmsFailure::unavailable("sms sending is not configured"));
        };
        let (url, key) = target.as_ref();
        let response = self
            .http
            .post(format!("{url}/sms/send"))
            .header("Authorization", key)
            .timeout(Duration::from_secs(120))
            .json(sms)
            .send()
            .await
            .map_err(|err| SmsFailure::unavailable(&err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            let sent: SentSms = response
                .json()
                .await
                .map_err(|err| SmsFailure::unavailable(&err.without_url().to_string()))?;
            return Ok(sent.ids);
        }
        let reply: SmsErrorReply = response.json().await.unwrap_or_default();
        Err(SmsFailure {
            status: status.as_u16(),
            error: if reply.error.is_empty() {
                "send_failed".to_string()
            } else {
                reply.error
            },
            sent_ids: reply.ids,
            unsent_s3_keys: reply.unsent_s3_keys,
        })
    }
}

/// Body of the `webhooks` `POST /telegram/crm-notify`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramCrmNotify {
    pub user_id: i32,
    pub deal_id: i32,
    /// `estimate_appointment_reminder`, ...
    pub notification_type: String,
    pub message: String,
    pub actor_name: Option<String>,
    pub customer_name: Option<String>,
}

/// Body of the `webhooks` `POST /sms/send`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingSms {
    /// `cloudtalk` or `ringcentral`.
    pub provider: String,
    pub company_id: i32,
    pub user_id: i32,
    pub agent_id: String,
    pub phone_digits: String,
    pub text: String,
    pub attachments: Vec<OutgoingSmsFile>,
    pub is_automated: bool,
    /// Kept across retries of one follow-up step so the provider drops a
    /// repeat; `None` for a fresh key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingSmsFile {
    pub s3_key: String,
    pub url: String,
    pub content_type: String,
    pub filename: String,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[derive(Deserialize)]
struct SentSms {
    ids: Vec<i64>,
}

#[derive(Deserialize, Default)]
struct SmsErrorReply {
    #[serde(default)]
    error: String,
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    unsent_s3_keys: Vec<String>,
}

/// A text that did not (fully) go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsFailure {
    /// The webhooks Lambda's status: 400 refused, 502 the provider failed.
    pub status: u16,
    /// A token the page translates (`agent_no_number`, `cloudtalk_…`).
    pub error: String,
    /// Messages already sent when a later one failed.
    pub sent_ids: Vec<i64>,
    /// Files of the messages that did not go out.
    pub unsent_s3_keys: Vec<String>,
}

impl SmsFailure {
    fn unavailable(detail: &str) -> Self {
        tracing::error!(detail, "sms send failed");
        Self {
            status: 502,
            error: "send_failed".to_string(),
            sent_ids: Vec::new(),
            unsent_s3_keys: Vec::new(),
        }
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
    /// Where replies go, when not to the sender.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_to: Vec<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
    /// A system notice: `body` is sent as is, without signature or opt-out line.
    pub transactional: bool,
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
