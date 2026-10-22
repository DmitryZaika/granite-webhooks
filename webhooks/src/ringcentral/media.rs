//! Pictures and files of an inbound `RingCentral` MMS. The API keeps them
//! (`POST /v1/sms/ringcentral/inbound-media`): the company's `RingCentral`
//! login and the bucket are there, not here.

use crate::ringcentral::schemas::MmsMedia;
use lambda_http::tracing;
use reqwest::Client;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct InboundMedia<'a> {
    company_id: i32,
    sms_id: u64,
    message_id: i64,
    extension_id: Option<&'a str>,
    files: &'a [MmsMedia],
}

/// Asks the API to store the files of SMS row `sms_id`. Failures are logged:
/// the text is already stored.
pub async fn store_inbound_media(
    company_id: i32,
    sms_id: u64,
    message_id: i64,
    extension_id: Option<&str>,
    files: &[MmsMedia],
) {
    if files.is_empty() {
        return;
    }
    let body = InboundMedia {
        company_id,
        sms_id,
        message_id,
        extension_id,
        files,
    };
    if let Err(error) = post(&body).await {
        tracing::error!(company_id, error, "RingCentral MMS files not stored");
    }
}

async fn post(body: &InboundMedia<'_>) -> Result<(), String> {
    let api_url = std::env::var("API_URL").map_err(|error| error.to_string())?;
    let lambda_key = std::env::var("LAMBDA_KEY").map_err(|error| error.to_string())?;
    let url = format!(
        "{}/v1/sms/ringcentral/inbound-media",
        api_url.trim_end_matches('/')
    );
    let response = Client::new()
        .post(url)
        .header("Authorization", lambda_key)
        .json(body)
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("status {}", response.status().as_u16()))
    }
}
