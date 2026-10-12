//! Calls to the `webhooks` Lambda for work it already owns (CloudTalk
//! contact sync). Configured with `WEBHOOKS_URL` (its function URL) and
//! `WEBHOOKS_KEY` (its `REMIX_KEY`); without them the calls are skipped.

use lambda_http::tracing;
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
        let Some(target) = &self.target else {
            return;
        };
        let (url, key) = target.as_ref();
        let result = self
            .http
            .post(format!("{url}/cloudtalk/sync/{company_id}/{customer_id}"))
            .header("Authorization", key)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::info!(status = %response.status(), customer_id, "cloudtalk sync skipped");
            }
            Err(err) => {
                tracing::warn!(error = %err.without_url(), customer_id, "cloudtalk sync failed");
            }
        }
    }
}
