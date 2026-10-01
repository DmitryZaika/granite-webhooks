use lambda_http::tracing;
use reqwest::Client;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmsFollowupCallCheckBody {
    pub company_id: i32,
    pub phone_digits: u64,
    pub call_id: u64,
    pub talking_time: u64,
    pub is_voicemail: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_link: Option<String>,
}

/// Fire-and-forget POST to the Remix app. Returns immediately; failures are logged.
pub fn spawn_sms_followup_call_check(body: SmsFollowupCallCheckBody) {
    tokio::spawn(async move {
        if let Err(error) = post_sms_followup_call_check(body).await {
            tracing::warn!(?error, "Failed to enqueue sms follow-up call check");
        }
    });
}

async fn post_sms_followup_call_check(body: SmsFollowupCallCheckBody) -> Result<(), String> {
    let app_url = std::env::var("APP_URL").map_err(|error| error.to_string())?;
    let lambda_key = std::env::var("LAMBDA_KEY").map_err(|error| error.to_string())?;
    let url = format!(
        "{}/api/sms-followups/call-check",
        app_url.trim_end_matches('/')
    );
    let response = Client::new()
        .post(url)
        .header("Authorization", lambda_key)
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status().as_u16()));
    }
    Ok(())
}
