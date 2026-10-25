//! `SignWell`, which hosts e-signatures for companies with its API key
//! (`company.signwell_api_key`, else `SIGNWELL_API_KEY`). Port of the CRM's
//! `signwell.server.ts`; the base URL is `AppState::signwell_base`.

use crate::signing_pdf::{FieldKind, SignatureField};
use base64::Engine;
use serde_json::{Value, json};
use std::time::Duration;

/// Larger files are sent by link instead of inline.
const BASE64_LIMIT_BYTES: usize = 6 * 1024 * 1024;

/// Preview points (72 DPI) to `SignWell` overlay pixels (96 DPI).
pub fn points_to_pixels(value: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    let pixels = (value * 96.0 / 72.0).round() as i64;
    pixels
}

/// Why `SignWell` refused, worded for the sender.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SignWellError(pub String);

/// The key to call `SignWell` with.
pub fn api_key(company_key: Option<&str>) -> Result<String, SignWellError> {
    company_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("SIGNWELL_API_KEY")
                .ok()
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty())
        })
        .ok_or_else(|| {
            SignWellError(
                "SignWell is not configured for this company. Add a SignWell API key in company settings."
                    .into(),
            )
        })
}

fn test_mode() -> bool {
    std::env::var("SIGNWELL_TEST_MODE").is_ok_and(|value| value == "true")
}

/// A document to create.
pub struct NewDocument<'a> {
    pub name: &'a str,
    pub filename: &'a str,
    pub file: &'a [u8],
    /// A link `SignWell` may fetch a large file from.
    pub file_url: Option<&'a str>,
    pub signer_name: &'a str,
    pub signer_email: &'a str,
    pub fields: &'a [SignatureField],
    pub requester_name: Option<&'a str>,
    pub requester_email: Option<&'a str>,
    pub reminders: bool,
    pub metadata: Value,
}

/// A created document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedDocument {
    pub id: String,
    pub signing_url: Option<String>,
}

/// A path segment: everything but RFC 3986's unreserved characters escaped.
fn encode(id: &str) -> String {
    const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(id, SEGMENT).to_string()
}

fn is_signing_url(value: &str) -> bool {
    reqwest::Url::parse(value)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some("www.signwell.com"))
}

/// The signer's `SignWell` link in a document payload.
pub fn signing_url_from_payload(payload: &Value) -> Option<String> {
    payload["recipients"]
        .as_array()?
        .iter()
        .find_map(|recipient| {
            ["embedded_signing_url", "signing_url"]
                .iter()
                .find_map(|key| {
                    recipient[*key]
                        .as_str()
                        .map(str::trim)
                        .filter(|url| is_signing_url(url))
                        .map(str::to_string)
                })
        })
}

async fn error_of(response: reqwest::Response) -> SignWellError {
    let status = response.status().as_u16();
    let payload: Value = response.json().await.unwrap_or(Value::Null);
    let text = |key: &str| {
        payload[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(error) = text("error") {
        if error == "not_authorized_error" {
            return SignWellError(if test_mode() {
                "SignWell rejected this test document. Check the company SignWell API key.".into()
            } else {
                "SignWell live documents are not enabled for this API key. Use a test-mode SignWell key, or enable live documents in SignWell.".into()
            });
        }
        return SignWellError(error);
    }
    SignWellError(text("message").unwrap_or_else(|| format!("SignWell request failed ({status})")))
}

/// The `SignWell` API of one base URL.
#[derive(Clone)]
pub struct SignWell {
    base: String,
    http: reqwest::Client,
}

impl SignWell {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: common::http::client(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str, key: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.base))
            .header("X-Api-Key", key)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(60))
    }

    /// Creates the document for the signer, who is not emailed by `SignWell`
    /// (the CRM's email carries the link).
    pub async fn create(
        &self,
        key: &str,
        document: &NewDocument<'_>,
    ) -> Result<CreatedDocument, SignWellError> {
        let file = if document.file.len() <= BASE64_LIMIT_BYTES {
            json!({
                "name": document.filename,
                "file_base64": base64::engine::general_purpose::STANDARD.encode(document.file),
            })
        } else {
            let url = document.file_url.filter(|url| url.starts_with("https://")).ok_or_else(|| {
                SignWellError("This PDF is too large to send for signature. Upload a smaller file and try again.".into())
            })?;
            json!({ "name": document.filename, "file_url": url })
        };
        let fields: Vec<Value> = document
            .fields
            .iter()
            .filter(|field| !field.is_stamped())
            .map(|field| {
                let kind = match field.kind {
                    FieldKind::Signature => "signature",
                    FieldKind::Date => "date",
                    FieldKind::Text => "text",
                    FieldKind::AutofillDateSigned => "autofill_date_signed",
                    // Always printed before sending: filtered out above.
                    FieldKind::Checkmark => "checkbox",
                };
                let mut value = json!({
                    "type": kind,
                    "required": field.required,
                    "page": field.page,
                    "x": points_to_pixels(field.x),
                    "y": points_to_pixels(field.y),
                    "width": points_to_pixels(field.width),
                    "height": points_to_pixels(field.height),
                    "recipient_id": "1",
                });
                if matches!(field.kind, FieldKind::Date | FieldKind::AutofillDateSigned) {
                    value["date_format"] = "MM/DD/YYYY".into();
                }
                value
            })
            .collect();
        let mut body = json!({
            "test_mode": test_mode(),
            "name": document.name,
            "draft": false,
            "reminders": document.reminders,
            "files": [file],
            "recipients": [{
                "id": "1",
                "name": document.signer_name,
                "email": document.signer_email,
                "send_email": false,
            }],
            "fields": [fields],
            "metadata": document.metadata,
        });
        if let Some(name) = document.requester_name.filter(|n| !n.is_empty()) {
            body["custom_requester_name"] = name.into();
        }
        if let Some(email) = document.requester_email.filter(|e| !e.is_empty()) {
            body["custom_requester_email"] = email.into();
        }
        let response = self
            .request(reqwest::Method::POST, "/documents", key)
            .json(&body)
            .send()
            .await
            .map_err(|_| {
                SignWellError(
                    "Could not create the signature request. The email was not sent.".into(),
                )
            })?;
        if !response.status().is_success() {
            return Err(error_of(response).await);
        }
        let payload: Value = response.json().await.unwrap_or(Value::Null);
        let id = payload["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| SignWellError("SignWell did not return a document id".into()))?
            .to_string();
        let signing_url = match signing_url_from_payload(&payload) {
            Some(url) => Some(url),
            None => self.signing_url(key, &id).await,
        };
        Ok(CreatedDocument { id, signing_url })
    }

    /// The signer's link, read back from the document.
    pub async fn signing_url(&self, key: &str, id: &str) -> Option<String> {
        let path = format!("/documents/{}", encode(id.trim()));
        let response = self
            .request(reqwest::Method::GET, &path, key)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        signing_url_from_payload(&response.json().await.ok()?)
    }

    /// Deletes the document so its link stops working; gone already is fine.
    /// The error is `SignWell`'s message, for the user.
    pub async fn delete(&self, key: &str, id: &str) -> Result<(), String> {
        let id = id.trim();
        if id.is_empty() {
            return Err("This request has no SignWell document.".into());
        }
        let path = format!("/documents/{}", encode(id));
        let response = self
            .request(reqwest::Method::DELETE, &path, key)
            .send()
            .await
            .map_err(|_| "Could not reach SignWell. The request was not cancelled.".to_string())?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body: Value = response.json().await.unwrap_or(Value::Null);
        Err(body["message"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .map_or_else(
                || format!("SignWell refused the cancellation ({})", status.as_u16()),
                str::to_string,
            ))
    }

    /// Deletes the document (best effort: a rollback after a failed send).
    pub async fn cancel(&self, key: &str, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let path = format!("/documents/{}", encode(id));
        let _ = self
            .request(reqwest::Method::DELETE, &path, key)
            .send()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_become_pixels() {
        assert_eq!(points_to_pixels(72.0), 96);
        assert_eq!(points_to_pixels(180.0), 240);
    }

    #[test]
    fn only_signwell_links_count() {
        let payload = json!({ "recipients": [
            { "signing_url": "https://evil.example/sign" },
            { "embedded_signing_url": "https://www.signwell.com/docs/abc" }
        ]});
        assert_eq!(
            signing_url_from_payload(&payload).as_deref(),
            Some("https://www.signwell.com/docs/abc")
        );
        assert_eq!(signing_url_from_payload(&json!({})), None);
    }
}
