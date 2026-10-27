//! Texts through `CloudTalk`. Port of the CRM's `cloudtalkSendSms.server.ts`
//! and `cloudtalkRequest`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
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

/// The production API.
pub const BASE_URL: &str = "https://my.cloudtalk.io/api";

/// The company's API key pair, base64 for Basic auth.
async fn auth_string(pool: &MySqlPool, company_id: i32) -> Result<String, ApiError> {
    let creds: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT cloudtalk_access_key, cloudtalk_access_secret FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
    .map_err(|err| ApiError {
        status: 500,
        message: Some(err.to_string()),
    })?;
    match creds {
        Some((Some(key), Some(secret))) if !key.is_empty() && !secret.is_empty() => {
            Ok(STANDARD.encode(format!("{key}:{secret}")))
        }
        _ => Err(ApiError {
            status: 401,
            message: Some("CloudTalk API credentials not found".into()),
        }),
    }
}

/// Where the API is and who calls it.
#[derive(Clone, Copy)]
pub struct Api<'a> {
    pub pool: &'a MySqlPool,
    pub client: &'a Client,
    /// [`BASE_URL`], or a stub in tests.
    pub base_url: &'a str,
    pub company_id: i32,
}

/// Calls `<base>/<path>` with the company's key, retrying 5xx answers and
/// rate limits.
pub async fn request(
    api: Api<'_>,
    method: Method,
    path: &str,
    body: Option<&Value>,
    idempotency_key: Option<&str>,
) -> Result<Value, ApiError> {
    let response = call(api, method, path, body, idempotency_key).await?;
    Ok(response.json().await.unwrap_or(Value::Null))
}

/// A file `CloudTalk` serves, such as a call recording.
#[derive(Debug)]
pub struct Raw {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

async fn raw(response: reqwest::Response) -> Result<Raw, ApiError> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = response.bytes().await.map_err(|err| ApiError {
        status: 502,
        message: Some(err.without_url().to_string()),
    })?;
    Ok(Raw {
        bytes: bytes.to_vec(),
        content_type,
    })
}

/// `GET <base>/<path>` as bytes.
pub async fn get_raw(api: Api<'_>, path: &str) -> Result<Raw, ApiError> {
    raw(call(api, Method::GET, path, None, None).await?).await
}

/// Whether `link` is `CloudTalk`'s: HTTPS on `cloudtalk.io` or a subdomain,
/// or on the API's own origin (`base_url`, a stand-in in tests). Links come
/// from request bodies too, and the company's key is sent to them.
fn is_cloudtalk_link(base_url: &str, link: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(link.trim()) else {
        return false;
    };
    if url.username() != "" || url.password().is_some() {
        return false;
    }
    let same_origin = reqwest::Url::parse(base_url).is_ok_and(|base| base.origin() == url.origin());
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    same_origin
        || (url.scheme() == "https"
            && url.port().is_none()
            && (host == "cloudtalk.io" || host.ends_with(".cloudtalk.io")))
}

/// A recording link `CloudTalk` gave: tried without credentials first, then
/// with the company's key. Links that are not `CloudTalk`'s are refused
/// (400) without being fetched.
pub async fn get_link(api: Api<'_>, link: &str) -> Result<Raw, ApiError> {
    if !is_cloudtalk_link(api.base_url, link) {
        return Err(ApiError {
            status: 400,
            message: Some("Not a CloudTalk recording link".into()),
        });
    }
    let link = link.trim();
    let failed = |err: reqwest::Error| ApiError {
        status: 502,
        message: Some(err.without_url().to_string()),
    };
    let open = api
        .client
        .get(link)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(failed)?;
    if open.status().is_success() {
        return raw(open).await;
    }
    let auth = auth_string(api.pool, api.company_id).await?;
    let signed = api
        .client
        .get(link)
        .header("Authorization", format!("Basic {auth}"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(failed)?;
    if !signed.status().is_success() {
        return Err(ApiError {
            status: signed.status().as_u16(),
            message: Some("Failed to fetch recording".into()),
        });
    }
    raw(signed).await
}

async fn call(
    api: Api<'_>,
    method: Method,
    path: &str,
    body: Option<&Value>,
    idempotency_key: Option<&str>,
) -> Result<reqwest::Response, ApiError> {
    let Api {
        pool,
        client,
        base_url,
        company_id,
    } = api;
    let auth = auth_string(pool, company_id).await?;
    let url = format!("{base_url}/{path}");
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
            return Ok(response);
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
pub async fn agent_sender(api: Api<'_>, agent_id: &str) -> Result<Option<String>, ApiError> {
    let path = format!("agents/index.json?id={}", urlencoding::encode(agent_id));
    let reply = request(api, Method::GET, &path, None, None).await?;
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
    api: Api<'_>,
    from: &str,
    to: &str,
    text: &str,
    idempotency_key: &str,
) -> Result<Option<i64>, ApiError> {
    let body = json!({ "sender": from, "recipient": to, "message": text });
    let reply = request(
        api,
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
    fn only_cloudtalk_links_are_fetched() {
        for link in [
            "https://my.cloudtalk.io/r/1.wav",
            "https://cloudtalk.io/r/1.wav",
            "https://Recordings.CloudTalk.io/r/1.wav?x=1",
        ] {
            assert!(is_cloudtalk_link(BASE_URL, link), "{link}");
        }
        for link in [
            "http://my.cloudtalk.io/r/1.wav",
            "https://my.cloudtalk.io:8443/r/1.wav",
            "https://cloudtalk.io.attacker.test/r.wav",
            "https://attacker.test/my.cloudtalk.io/r.wav",
            "https://notcloudtalk.io/r.wav",
            "https://user:pw@my.cloudtalk.io/r.wav",
            "https://169.254.169.254/latest/meta-data",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(!is_cloudtalk_link(BASE_URL, link), "{link}");
        }
        assert!(is_cloudtalk_link(
            "http://127.0.0.1:9123/api",
            "http://127.0.0.1:9123/r/1.wav"
        ));
        assert!(!is_cloudtalk_link(
            "http://127.0.0.1:9123/api",
            "http://127.0.0.1:9124/r/1.wav"
        ));
    }

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
