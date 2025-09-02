use std::default::Default;
use std::time::Duration;

use reqwest::{Client as HttpClient, header::CONTENT_TYPE};

use crate::posthog::{Error, PostHogEvent};

const API_ENDPOINT: &str = "https://us.i.posthog.com/i/v0/e/";

pub struct ClientOptions {
    api_endpoint: &'static str,

    request_timeout_seconds: u64,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            api_endpoint: API_ENDPOINT,
            request_timeout_seconds: 30,
        }
    }
}

/// A [`Client`] facilitates interactions with the `PostHog` API over HTTP.
pub struct Client {
    options: ClientOptions,
    client: HttpClient,
}

/// This function constructs a new client using the options provided.
pub async fn client() -> Client {
    let options = ClientOptions::default();
    let client = HttpClient::builder()
        .timeout(Duration::from_secs(options.request_timeout_seconds))
        .build()
        .unwrap(); // Unwrap here is as safe as `HttpClient::new`
    Client { options, client }
}

impl Client {
    /// Capture the provided event, sending it to `PostHog`.
    pub async fn capture(&self, event: PostHogEvent) -> Result<reqwest::Response, Error> {
        let payload =
            serde_json::to_string(&event).map_err(|e| Error::Serialization(e.to_string()))?;
        println!("Payload: {payload}");

        self.client
            .post(self.options.api_endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Connection(e.to_string()))
    }
}
