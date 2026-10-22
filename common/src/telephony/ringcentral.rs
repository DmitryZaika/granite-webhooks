//! Texts through `RingCentral`: plain SMS, and MMS with the pictures attached.
//! Port of the CRM's `ringcentralSendSms.server.ts` and `ringcentralRequest`.

use super::ringcentral_auth::{AuthError, access_token, forget};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::time::Duration;

const SERVER_RETRIES: usize = 3;
const SERVER_BACKOFF_SECS: [u64; 3] = [2, 4, 8];
const RATE_LIMIT_RETRIES: usize = 2;
/// Longest single wait on a 429 (`RingCentral` asks for 60 s).
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 30;

/// A failed call: the HTTP status and `RingCentral`'s own message.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: Option<String>,
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let status = match &error {
            AuthError::Token { status, .. } => *status,
            AuthError::NoCredentials => 401,
            _ => 500,
        };
        Self {
            status,
            message: Some(error.to_string()),
        }
    }
}

/// One file of an MMS.
#[derive(Debug, Clone)]
pub struct MmsFile {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub filename: String,
}

/// What a request carries.
pub enum Body<'a> {
    Empty,
    Json(&'a Value),
    Mms {
        request: &'a Value,
        files: &'a [MmsFile],
    },
}

fn multipart(request: &Value, files: &[MmsFile]) -> Form {
    let mut form = Form::new().part(
        "json",
        Part::text(request.to_string())
            .file_name("request.json")
            .mime_str("application/json")
            .unwrap_or_else(|_| Part::text(request.to_string())),
    );
    for file in files {
        let part = Part::bytes(file.bytes.clone()).file_name(file.filename.clone());
        let part = part
            .mime_str(&file.content_type)
            .unwrap_or_else(|_| Part::bytes(file.bytes.clone()).file_name(file.filename.clone()));
        form = form.part("attachment", part);
    }
    form
}

fn api_message(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    parsed
        .get("message")
        .or_else(|| parsed.get("error_description"))
        .or_else(|| parsed.pointer("/errors/0/message"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Calls `restapi/v1.0/<path>`: retries a refused token, 5xx answers and
/// rate limits (honouring `Retry-After`, up to 30 s at a time).
pub async fn request(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    method: Method,
    path: &str,
    body: &Body<'_>,
    idempotency_key: Option<&str>,
) -> Result<Value, ApiError> {
    let response = call(
        pool,
        client,
        company_id,
        method,
        path,
        body,
        idempotency_key,
        "application/json",
    )
    .await?;
    let text = response.text().await.unwrap_or_default();
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// `GET restapi/v1.0/<path>` as bytes and their content type, for call
/// recordings and voicemail (which refuse an `application/json` `Accept`).
pub async fn get_raw(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    path: &str,
) -> Result<(Vec<u8>, String), ApiError> {
    let response = call(
        pool,
        client,
        company_id,
        Method::GET,
        path,
        &Body::Empty,
        None,
        "*/*",
    )
    .await?;
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
    Ok((bytes.to_vec(), content_type))
}

#[allow(clippy::too_many_arguments)]
async fn call(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    method: Method,
    path: &str,
    body: &Body<'_>,
    idempotency_key: Option<&str>,
    accept: &str,
) -> Result<reqwest::Response, ApiError> {
    let mut server_attempt = 0;
    let mut rate_attempt = 0;
    loop {
        let token = access_token(pool, client, company_id).await?;
        let url = format!(
            "{}/restapi/v1.0/{}",
            token.server_url,
            path.trim_start_matches('/')
        );
        let mut builder = client
            .request(method.clone(), &url)
            .bearer_auth(&token.token)
            .header("Accept", accept)
            .timeout(Duration::from_secs(60));
        if let Some(key) = idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder = match body {
            Body::Empty => builder,
            Body::Json(value) => builder.json(value),
            Body::Mms { request, files } => builder.multipart(multipart(request, files)),
        };
        let response = builder.send().await.map_err(|err| ApiError {
            status: 502,
            message: Some(err.without_url().to_string()),
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status == StatusCode::UNAUTHORIZED && server_attempt < SERVER_RETRIES {
            forget(company_id);
            server_attempt += 1;
            continue;
        }
        if status == StatusCode::TOO_MANY_REQUESTS && rate_attempt < RATE_LIMIT_RETRIES {
            let wait = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|secs| *secs > 0)
                .unwrap_or(5)
                .min(MAX_RATE_LIMIT_WAIT_SECS);
            tracing::warn!(company_id, path, wait, "RingCentral rate limited");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            rate_attempt += 1;
            continue;
        }
        if status.is_server_error() && server_attempt < SERVER_RETRIES {
            tokio::time::sleep(Duration::from_secs(SERVER_BACKOFF_SECS[server_attempt])).await;
            server_attempt += 1;
            continue;
        }
        let text = response.text().await.unwrap_or_default();
        tracing::error!(company_id, path, %status, body = %text.chars().take(1000).collect::<String>(), "RingCentral API error");
        return Err(ApiError {
            status: status.as_u16(),
            message: api_message(&text),
        });
    }
}

/// The extension's first phone number (its texting number), if any.
pub async fn extension_sender(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    extension_id: &str,
) -> Result<Option<String>, ApiError> {
    let path = format!("account/~/extension/{extension_id}/phone-number?perPage=100");
    let numbers = match request(
        pool,
        client,
        company_id,
        Method::GET,
        &path,
        &Body::Empty,
        None,
    )
    .await
    {
        Ok(numbers) => numbers,
        Err(error) if error.status == 404 => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(numbers["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|record| record["phoneNumber"].as_str())
        .map(str::trim)
        .find(|number| !number.is_empty())
        .map(str::to_string))
}

/// One message to send; with files it goes as a real MMS.
pub struct Outgoing<'a> {
    pub extension_id: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub text: &'a str,
    pub files: &'a [MmsFile],
    pub idempotency_key: &'a str,
}

/// A positive numeric id (`coerceId`).
fn coerce_id(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().filter(|id| *id > 0),
        Value::String(text) => text.parse::<i64>().ok().filter(|id| *id > 0),
        _ => None,
    }
}

/// Sends one message; the provider's message id on success.
pub async fn send(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
    message: &Outgoing<'_>,
) -> Result<Option<i64>, ApiError> {
    let mut request_body = json!({
        "from": { "phoneNumber": message.from },
        "to": [{ "phoneNumber": message.to }],
    });
    if !message.text.is_empty() || message.files.is_empty() {
        request_body["text"] = Value::String(message.text.to_string());
    }
    let (path, body) = if message.files.is_empty() {
        ("sms", Body::Json(&request_body))
    } else {
        (
            "mms",
            Body::Mms {
                request: &request_body,
                files: message.files,
            },
        )
    };
    let reply = request(
        pool,
        client,
        company_id,
        Method::POST,
        &format!("account/~/extension/{}/{path}", message.extension_id),
        &body,
        Some(message.idempotency_key),
    )
    .await?;
    let status = reply["messageStatus"].as_str().unwrap_or_default();
    if status.to_lowercase().contains("fail") {
        return Err(ApiError {
            status: 200,
            message: Some(status.to_string()),
        });
    }
    Ok(coerce_id(&reply["id"]))
}

/// The token the CRM shows for a failed SMS.
pub fn sms_error_token(error: &ApiError) -> String {
    let message = error.message.as_deref().unwrap_or_default().to_lowercase();
    if message.contains("not enough credits") || message.contains("insufficient") {
        return "ringcentral_insufficient_funds".into();
    }
    if message.contains("daily sms limit") || message.contains("monthly sms limit") {
        return "ringcentral_limit_exceeded".into();
    }
    if message.contains("international") {
        return "ringcentral_country_not_allowed".into();
    }
    if message.contains("invalid") || message.contains("phone number") {
        return "ringcentral_invalid_phone".into();
    }
    if message.contains("does not support sms") || message.contains("feature") {
        return "ringcentral_bad_number_config".into();
    }
    format!("ringcentral_{}", error.status)
}

/// MMS refusals (not rate limits) usually mean the number cannot send
/// pictures.
pub fn mms_error_token(error: &ApiError) -> String {
    let token = sms_error_token(error);
    let refused = token == "ringcentral_bad_number_config"
        || (token.starts_with("ringcentral_4")
            && token.len() == "ringcentral_400".len()
            && token[12..].chars().all(|c| c.is_ascii_digit()));
    if refused && token != "ringcentral_429" {
        "ringcentral_mms_not_supported".into()
    } else {
        token
    }
}

/// `RingCentral`'s cap for one MMS is 1.5 MB; stay a little under it.
pub const MMS_MAX_TOTAL_BYTES: usize = 1_450_000;
/// What carriers reliably deliver whole.
pub const MMS_CARRIER_SAFE_BYTES: usize = 600_000;
pub const MMS_MAX_ATTACHMENTS: usize = 10;

/// Splits files, in order, into messages carriers deliver whole (up to
/// 600 KB together; a bigger file up to 1.5 MB alone). Larger ones come back
/// separately, to be sent as links. Port of `planMmsBatches`.
pub fn plan_batches<T>(items: Vec<(T, usize)>) -> (Vec<Vec<T>>, Vec<T>) {
    let mut batches = Vec::new();
    let mut too_large = Vec::new();
    let mut current: Vec<T> = Vec::new();
    let mut bytes = 0;
    for (item, size) in items {
        if size > MMS_MAX_TOTAL_BYTES {
            too_large.push(item);
            continue;
        }
        if !current.is_empty()
            && (bytes + size > MMS_CARRIER_SAFE_BYTES || current.len() >= MMS_MAX_ATTACHMENTS)
        {
            batches.push(std::mem::take(&mut current));
            bytes = 0;
        }
        current.push(item);
        bytes += size;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    (batches, too_large)
}

/// `RingCentral` refuses a file whose extension does not match its type.
pub fn mms_filename(filename: &str, content_type: &str) -> String {
    let ext = match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/csv" => "csv",
        _ => return filename.to_string(),
    };
    let base = match filename.rfind('.') {
        Some(dot) if !filename[dot..].contains(['/', '\\']) => &filename[..dot],
        _ => filename,
    };
    let base = base.trim();
    format!("{}.{ext}", if base.is_empty() { "file" } else { base })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_follow_carrier_limits() {
        let (batches, too_large) = plan_batches(vec![
            ("a", 300_000),
            ("b", 250_000),
            ("c", 100_000),
            ("d", 2_000_000),
        ]);
        assert_eq!(batches, vec![vec!["a", "b"], vec!["c"]]);
        assert_eq!(too_large, vec!["d"]);
        let (batches, _) = plan_batches(vec![("big", 1_000_000), ("small", 10)]);
        assert_eq!(batches, vec![vec!["big"], vec!["small"]]);
    }

    #[test]
    fn file_names_match_their_type() {
        assert_eq!(mms_filename("slab.jpg", "image/png"), "slab.png");
        assert_eq!(mms_filename("quote", "application/pdf"), "quote.pdf");
        assert_eq!(mms_filename(".jpg", "image/jpeg"), "file.jpg");
        assert_eq!(mms_filename("sheet.xlsx", "application/zip"), "sheet.xlsx");
    }

    #[test]
    fn error_tokens() {
        let error = |status, message: &str| ApiError {
            status,
            message: Some(message.to_string()),
        };
        assert_eq!(
            sms_error_token(&error(400, "Invalid phone number")),
            "ringcentral_invalid_phone"
        );
        assert_eq!(sms_error_token(&error(503, "down")), "ringcentral_503");
        assert_eq!(
            mms_error_token(&error(400, "nope")),
            "ringcentral_mms_not_supported"
        );
        assert_eq!(mms_error_token(&error(429, "slow")), "ringcentral_429");
        assert_eq!(mms_error_token(&error(503, "down")), "ringcentral_503");
    }
}

/// `RingCentral`'s ceiling for `WebHook` subscriptions (20 years).
const SUBSCRIPTION_EXPIRES_IN_SECONDS: u64 = 630_720_000;

/// What [`ensure_sms_subscription`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionOutcome {
    Created,
    Renewed,
    /// `LAMBDA_URL` or `RINGCENTRAL_WEBHOOK_TOKEN` is not set, or the
    /// account has no enabled user extensions.
    Skipped,
}

/// Where `RingCentral` pushes the company's new texts
/// (`/ringcentral/notify/{company_id}` on the webhooks Lambda).
pub fn notify_address(company_id: i32) -> Option<String> {
    let base = std::env::var("LAMBDA_URL").ok()?;
    let base = base.trim().trim_end_matches('/');
    (!base.is_empty()).then(|| format!("{base}/ringcentral/notify/{company_id}"))
}

/// The SMS event filters of each extension: inbound texts (`instant`), and
/// any message-store change, which is how a text sent from the `RingCentral`
/// app shows up.
pub fn sms_event_filters(extension_ids: &[String]) -> Vec<String> {
    extension_ids
        .iter()
        .flat_map(|id| {
            [
                format!("/restapi/v1.0/account/~/extension/{id}/message-store/instant?type=SMS"),
                format!("/restapi/v1.0/account/~/extension/{id}/message-store"),
            ]
        })
        .collect()
}

/// Creates the company's SMS webhook subscription, or renews it with the
/// current extensions so reps added since are covered too. Without it
/// inbound texts never reach the CRM. Port of
/// `ensureRingCentralSmsSubscription`.
pub async fn ensure_sms_subscription(
    pool: &MySqlPool,
    client: &Client,
    company_id: i32,
) -> Result<SubscriptionOutcome, ApiError> {
    let Some(address) = notify_address(company_id) else {
        return Ok(SubscriptionOutcome::Skipped);
    };
    // RingCentral rejects hyphens; the webhook parses the 32-hex form as the same UUID.
    let token = std::env::var("RINGCENTRAL_WEBHOOK_TOKEN")
        .unwrap_or_default()
        .trim()
        .replace('-', "");
    if token.is_empty() {
        return Ok(SubscriptionOutcome::Skipped);
    }
    let extensions = request(
        pool,
        client,
        company_id,
        Method::GET,
        "account/~/extension?type=User&status=Enabled&perPage=1000",
        &Body::Empty,
        None,
    )
    .await?;
    let extension_ids: Vec<String> = extensions["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|record| {
            record["id"]
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| record["id"].as_str().map(str::to_string))
        })
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .collect();
    if extension_ids.is_empty() {
        return Ok(SubscriptionOutcome::Skipped);
    }
    let subscriptions = request(
        pool,
        client,
        company_id,
        Method::GET,
        "subscription",
        &Body::Empty,
        None,
    )
    .await?;
    let existing = subscriptions["records"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|record| record["deliveryMode"]["address"].as_str() == Some(address.as_str()))
        .and_then(|record| record["id"].as_str().map(str::to_string));
    let body = json!({
        "eventFilters": sms_event_filters(&extension_ids),
        "deliveryMode": { "transportType": "WebHook", "address": address, "verificationToken": token },
        "expiresIn": SUBSCRIPTION_EXPIRES_IN_SECONDS,
    });
    let (method, path, outcome) = match &existing {
        Some(id) => (
            Method::PUT,
            format!("subscription/{id}"),
            SubscriptionOutcome::Renewed,
        ),
        None => (
            Method::POST,
            "subscription".to_string(),
            SubscriptionOutcome::Created,
        ),
    };
    request(
        pool,
        client,
        company_id,
        method,
        &path,
        &Body::Json(&body),
        None,
    )
    .await?;
    Ok(outcome)
}
