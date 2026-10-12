//! Texts through `CloudTalk`. Port of the CRM's `cloudtalkSendSms.server.ts`
//! and `cloudtalkRequest`.

use crate::crud::cloudtalk::get_auth_string;
use lambda_http::tracing;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SERVER_RETRIES: usize = 3;
const SERVER_BACKOFF_SECS: [u64; 3] = [2, 4, 8];
const RATE_LIMIT_RETRIES: usize = 3;
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 30;

/// A failed call: the HTTP status and `CloudTalk`'s own message.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: Option<String>,
}

fn api_message(body: &Value) -> Option<String> {
    let data = &body["responseData"];
    let node = if data.is_array() { &data[0] } else { data };
    node["message"].as_str().map(str::to_string)
}

/// Seconds to wait on a 429: until `X-CloudTalkAPI-ResetTime` (epoch
/// seconds) plus one, else five; at most 30.
fn rate_limit_wait(reset: Option<u64>) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    reset
        .filter(|reset| *reset > 0)
        .map_or(5, |reset| reset.saturating_sub(now) + 1)
        .min(MAX_RATE_LIMIT_WAIT_SECS)
}

/// Calls `<base>/<path>` with the company's key, retrying 5xx answers and
/// rate limits.
pub async fn request(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    method: Method,
    path: &str,
    body: Option<&Value>,
    idempotency_key: Option<&str>,
) -> Result<Value, ApiError> {
    let auth = get_auth_string(pool, u64::try_from(company_id).unwrap_or_default())
        .await
        .map_err(|err| ApiError {
            status: 401,
            message: Some(err.to_string()),
        })?;
    let url = format!("{}/{path}", super::api::base_url());
    let mut server_attempt = 0;
    let mut rate_attempt = 0;
    loop {
        let mut builder = client
            .request(method.clone(), &url)
            .header("Authorization", format!("Basic {auth}"))
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(30));
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            builder = builder.json(body);
        }
        let response = builder.send().await.map_err(|err| ApiError {
            status: 502,
            message: Some(err.without_url().to_string()),
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await.unwrap_or(Value::Null));
        }
        if status == StatusCode::TOO_MANY_REQUESTS && rate_attempt < RATE_LIMIT_RETRIES {
            let reset = response
                .headers()
                .get("X-CloudTalkAPI-ResetTime")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            tokio::time::sleep(Duration::from_secs(rate_limit_wait(reset))).await;
            rate_attempt += 1;
            continue;
        }
        if status.is_server_error() && server_attempt < SERVER_RETRIES {
            tokio::time::sleep(Duration::from_secs(SERVER_BACKOFF_SECS[server_attempt])).await;
            server_attempt += 1;
            continue;
        }
        let text = response.text().await.unwrap_or_default();
        tracing::error!(company_id, path, %status, body = %text.chars().take(1000).collect::<String>(), "CloudTalk API error");
        return Err(ApiError {
            status: status.as_u16(),
            message: serde_json::from_str(&text)
                .ok()
                .as_ref()
                .and_then(api_message),
        });
    }
}

/// The agent's own number: its default, else the first associated one.
pub async fn agent_sender(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    agent_id: &str,
) -> Result<Option<String>, ApiError> {
    let path = format!("agents/index.json?id={}", urlencoding::encode(agent_id));
    let reply = request(pool, client, company_id, Method::GET, &path, None, None).await?;
    // The `id` filter may be ignored; rows nest under `Agent`.
    let agent = reply["responseData"]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| &item["Agent"])
        .find(|agent| match &agent["id"] {
            Value::Number(id) => id.to_string() == agent_id,
            Value::String(id) => id == agent_id,
            _ => false,
        });
    let Some(agent) = agent else {
        return Ok(None);
    };
    let first = agent["associated_numbers"][0].as_str();
    Ok(agent["default_number"]
        .as_str()
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .or_else(|| first.map(str::trim).filter(|number| !number.is_empty()))
        .map(str::to_string))
}

/// The error a 200 answer reports (`success: false`, or a status of 400+).
fn reported_error(reply: &Value) -> Option<String> {
    let data = &reply["responseData"];
    if let Some(first) = data.as_array().and_then(|items| items.first()) {
        let status = first["status"].as_i64()?;
        if status < 400 {
            return None;
        }
        return Some(
            first["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map_or_else(|| format!("cloudtalk_{status}"), str::to_string),
        );
    }
    let failed = data["success"] == json!(false) || data["success"] == json!("false");
    if failed {
        let message = data["message"]
            .as_str()
            .filter(|m| !m.is_empty())
            .or_else(|| data["data"].as_str().filter(|m| !m.is_empty()))
            .unwrap_or("sms_send_failed");
        return Some(message.to_string());
    }
    match data["status"].as_i64() {
        Some(status) if status >= 400 => Some(
            data["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map_or_else(|| format!("cloudtalk_{status}"), str::to_string),
        ),
        _ => None,
    }
}

/// The message id, when the answer carries one (it usually does not).
fn message_id(reply: &Value) -> Option<i64> {
    let data = &reply["responseData"];
    let node = if data.is_array() {
        &data[0]
    } else if data["data"].is_array() {
        &data["data"][0]
    } else if data["data"].is_object() {
        &data["data"]
    } else {
        data
    };
    node["id"].as_i64().filter(|id| *id > 0)
}

/// Sends one text; the provider's message id when it gives one.
pub async fn send(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    from: &str,
    to: &str,
    text: &str,
    idempotency_key: &str,
) -> Result<Option<i64>, ApiError> {
    let body = json!({ "sender": from, "recipient": to, "message": text });
    let reply = request(
        pool,
        client,
        company_id,
        Method::POST,
        "sms/send.json",
        Some(&body),
        Some(idempotency_key),
    )
    .await?;
    if let Some(message) = reported_error(&reply) {
        return Err(ApiError {
            status: 200,
            message: Some(message),
        });
    }
    Ok(message_id(&reply))
}

/// The token the CRM shows for a failed send.
pub fn error_token(error: &ApiError) -> String {
    let message = error.message.as_deref().unwrap_or_default().to_lowercase();
    let token = if message.contains("insufficient funds") {
        "cloudtalk_insufficient_funds"
    } else if message.contains("limit exceeded") {
        "cloudtalk_limit_exceeded"
    } else if message.contains("not allowed country") {
        "cloudtalk_country_not_allowed"
    } else if message.contains("unknown number") || message.contains("invalid phone") {
        "cloudtalk_invalid_phone"
    } else if message.contains("bad number configuration") {
        "cloudtalk_bad_number_config"
    } else if message.contains("agent") {
        "cloudtalk_agent_unavailable"
    } else {
        return format!("cloudtalk_{}", error.status);
    };
    token.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_errors_from_200_answers() {
        assert_eq!(
            reported_error(
                &json!({ "responseData": { "success": false, "data": "Insufficient funds" } })
            ),
            Some("Insufficient funds".into())
        );
        assert_eq!(
            reported_error(&json!({ "responseData": [{ "status": 406, "message": "" }] })),
            Some("cloudtalk_406".into())
        );
        assert_eq!(
            reported_error(&json!({ "responseData": { "success": true, "data": { "id": 5 } } })),
            None
        );
        assert_eq!(
            message_id(&json!({ "responseData": { "success": true, "data": { "id": 5 } } })),
            Some(5)
        );
        assert_eq!(
            message_id(&json!({ "responseData": { "success": true } })),
            None
        );
    }

    #[test]
    fn maps_errors_to_tokens() {
        let error = |message: &str| ApiError {
            status: 200,
            message: Some(message.into()),
        };
        assert_eq!(
            error_token(&error("Insufficient funds")),
            "cloudtalk_insufficient_funds"
        );
        assert_eq!(
            error_token(&error("Agent is busy")),
            "cloudtalk_agent_unavailable"
        );
        assert_eq!(error_token(&error("???")), "cloudtalk_200");
        assert_eq!(rate_limit_wait(None), 5);
    }
}
