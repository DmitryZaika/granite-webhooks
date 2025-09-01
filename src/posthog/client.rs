use std::time::Duration;

use reqwest::{Client as HttpClient, header::CONTENT_TYPE};

use crate::posthog::error::Error;
use crate::posthog::event::{Event, InnerEvent};

const API_ENDPOINT: &str = "https://us.i.posthog.com/i/v0/e/";

pub struct ClientOptions {
    api_endpoint: &'static str,
    api_key: String,

    request_timeout_seconds: u64,
}

impl ClientOptions {
    pub const fn from(api_key: String) -> Self {
        Self {
            api_endpoint: API_ENDPOINT,
            api_key,
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
pub async fn client(api_key: String) -> Client {
    let options = ClientOptions::from(api_key);
    let client = HttpClient::builder()
        .timeout(Duration::from_secs(options.request_timeout_seconds))
        .build()
        .unwrap(); // Unwrap here is as safe as `HttpClient::new`
    Client { options, client }
}

impl Client {
    /// Capture the provided event, sending it to `PostHog`.
    pub async fn capture(&self, event: Event) -> Result<reqwest::Response, Error> {
        let inner_event = InnerEvent::new(event, self.options.api_key.clone());

        let payload =
            serde_json::to_string(&inner_event).map_err(|e| Error::Serialization(e.to_string()))?;
        println!("Payload: {}", payload);

        self.client
            .post(self.options.api_endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Connection(e.to_string()))
    }
}
