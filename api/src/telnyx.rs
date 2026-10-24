//! Telnyx, for Granite Phone: the REST client, a TeXML builder and the
//! webhook signature check.
//!
//! - [`Telnyx`]: the REST API with our key (`TELNYX_API_KEY`;
//!   `TELNYX_API_BASE` points it at a stub in tests). Telephony credentials
//!   (one per phone, its SIP identity) and their login JWTs, Call Control
//!   commands on the app's parked calls (transfer, hangup), recordings,
//!   TeXML calls our server places (call via my cell, test calls, cell
//!   codes) and redirects, and the numbers of a TeXML app. Without a key
//!   every call answers [`TelnyxError::NotConfigured`].
//! - [`Response`] / [`Verb`]: TeXML answers; every attribute and text is
//!   XML-escaped.
//! - [`verify_signature`]: Telnyx's Ed25519 webhook signature
//!   (`telnyx-signature-ed25519` over `{telnyx-timestamp}|{raw body}`).
//!
//! Nothing here logs phone numbers, tokens or Telnyx's error details.
//! Texting (`POST /messages`, inbound MMS files, the messaging webhook's
//! events) is in [`messaging`]; texting registration (10DLC brands,
//! campaigns, number assignments) in [`tendlc`].

pub mod messaging;
pub mod tendlc;

use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub mod porting;

/// Telnyx's API root.
pub const BASE_URL: &str = "https://api.telnyx.com/v2";
/// Every Telnyx request gives up after this long.
const TIMEOUT: Duration = Duration::from_secs(10);
/// ... but a recording download after this long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Numbers per page, and pages at most, read by
/// [`Telnyx::list_numbers_for_connection`].
const NUMBERS_PAGE_SIZE: u32 = 250;
const MAX_NUMBER_PAGES: u32 = 20;
/// How far a signed webhook's `telnyx-timestamp` may be from our clock.
pub const SIGNATURE_WINDOW_SECS: u64 = 300;
/// The DER prefix of an Ed25519 public key in `SubjectPublicKeyInfo` form,
/// in case the key was pasted that way instead of as the raw 32 bytes.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// What stays as it is when an id goes in a URL path (Call Control ids look
/// like `v3:abc-123_x`); everything else is percent-encoded.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b':');

/// The Telnyx REST API, with our key.
#[derive(Clone)]
pub struct Telnyx {
    key: Option<Arc<str>>,
    base: Arc<str>,
    http: reqwest::Client,
}

/// Why a Telnyx request failed.
#[derive(Debug, thiserror::Error)]
pub enum TelnyxError {
    /// `TELNYX_API_KEY` is not set: nothing was sent.
    #[error("TELNYX_API_KEY is not set")]
    NotConfigured,
    /// The request did not complete (connection refused, timeout, ...).
    #[error("Telnyx request failed: {0}")]
    Request(String),
    /// Telnyx answered with an error status; `code` is the first of its
    /// `errors[].code` (for example `90018`: the call has already ended).
    #[error("Telnyx answered {status}")]
    Status { status: u16, code: Option<String> },
    /// A success answer without what we asked for.
    #[error("unexpected Telnyx answer: {0}")]
    Unexpected(&'static str),
}

impl TelnyxError {
    /// The HTTP status Telnyx answered, when it answered.
    pub const fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Telnyx's error code, when it gave one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Status { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}

/// A telephony credential: the SIP identity of one phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    /// Its Telnyx id (to mint login JWTs, extend or delete it).
    pub id: String,
    /// `gencred...`: the phone's SIP user (`sip:{sip_username}@sip.telnyx.com`).
    pub sip_username: String,
}

/// A call recording on Telnyx.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recording {
    /// Where to download the MP3 (a short-lived link; no API key needed).
    pub mp3_url: Option<String>,
    /// Its length, rounded to the second.
    pub duration_secs: Option<i64>,
    /// The call it is of, as Telnyx names it (whichever ids it gives): the
    /// leg's Call Control id (a TeXML `CallSid`), its leg id, its call
    /// session, and its connection.
    pub call_control_id: Option<String>,
    pub call_leg_id: Option<String>,
    pub call_session_id: Option<String>,
    pub connection_id: Option<String>,
}

impl Recording {
    /// The ids of the call it is of (none when Telnyx gave none).
    pub fn call_ids(&self) -> impl Iterator<Item = &str> {
        [
            &self.call_control_id,
            &self.call_leg_id,
            &self.call_session_id,
        ]
        .into_iter()
        .filter_map(|id| id.as_deref())
    }
}

/// One number of a connection (a TeXML app).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionNumber {
    /// Telnyx's id of the number.
    pub id: String,
    /// The number, E.164.
    pub phone_number: String,
}

/// `POST /calls/{call_control_id}/actions/transfer`: bridges the app's
/// parked call to `to`. Unset fields are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Transfer {
    /// The destination, E.164 (or a SIP URI).
    pub to: String,
    /// The caller ID shown to the destination, E.164.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Played to the destination when it answers, before bridging (the
    /// recording notice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<String>,
    /// `record-from-answer` to record the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    /// `single` or `dual`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_channels: Option<String>,
    /// `mp3` or `wav`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_format: Option<String>,
    /// How long the destination may ring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
    /// Where Telnyx sends this call's later events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// Telnyx ignores a second command with the same id on the same call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    /// Echoed in every later event of the call; base64.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_state: Option<String>,
    /// Echoed in every event of the destination's new leg (its
    /// `call.answered` ...); base64.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_leg_client_state: Option<String>,
}

impl Telnyx {
    /// No key: every call answers [`TelnyxError::NotConfigured`].
    pub fn disabled() -> Self {
        Self::new(None, BASE_URL)
    }

    /// `TELNYX_API_KEY`, and `TELNYX_API_BASE` (default [`BASE_URL`]).
    pub fn from_env() -> Self {
        let base = std::env::var("TELNYX_API_BASE")
            .ok()
            .filter(|base| !base.trim().is_empty())
            .unwrap_or_else(|| BASE_URL.to_string());
        Self::new(std::env::var("TELNYX_API_KEY").ok().as_deref(), base.trim())
    }

    /// `base` is the API root, `https://api.telnyx.com/v2` (a stub in tests).
    pub fn new(key: Option<&str>, base: &str) -> Self {
        Self {
            key: key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(Arc::from),
            base: Arc::from(base.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("HTTP client builds"),
        }
    }

    /// A key is set.
    pub const fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response, TelnyxError> {
        let key = self.key.as_deref().ok_or(TelnyxError::NotConfigured)?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(key);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        let code = match &answer["errors"][0]["code"] {
            Value::String(code) => Some(code.clone()),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        };
        Err(TelnyxError::Status { status, code })
    }

    async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, TelnyxError> {
        self.send(method, path, body)
            .await?
            .json()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))
    }

    /// A delete that finds nothing is done already.
    async fn delete(&self, path: &str) -> Result<(), TelnyxError> {
        match self.send(Method::DELETE, path, None).await {
            Ok(_) => Ok(()),
            Err(err) if err.status() == Some(404) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Creates a phone's telephony credential on the company's credential
    /// connection (`POST /telephony_credentials`); it stops working at
    /// `expires_at` unless extended.
    pub async fn create_credential(
        &self,
        connection_id: &str,
        name: &str,
        tag: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Credential, TelnyxError> {
        let body = json!({
            "connection_id": connection_id,
            "name": name,
            "tag": tag,
            "expires_at": telnyx_time(expires_at),
        });
        let answer = self
            .json(Method::POST, "/telephony_credentials", Some(&body))
            .await?;
        let text = |field: &str| {
            answer["data"][field]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Ok(Credential {
            id: text("id").ok_or(TelnyxError::Unexpected("credential without id"))?,
            sip_username: text("sip_username")
                .ok_or(TelnyxError::Unexpected("credential without sip_username"))?,
        })
    }

    /// Moves a credential's expiry (`PATCH /telephony_credentials/{id}`).
    pub async fn set_credential_expiry(
        &self,
        id: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), TelnyxError> {
        let body = json!({ "expires_at": telnyx_time(expires_at) });
        self.send(
            Method::PATCH,
            &format!("/telephony_credentials/{}", segment(id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// Deletes a credential, so its phone can no longer register or call
    /// (`DELETE /telephony_credentials/{id}`); a missing one is fine.
    pub async fn delete_credential(&self, id: &str) -> Result<(), TelnyxError> {
        self.delete(&format!("/telephony_credentials/{}", segment(id)))
            .await
    }

    /// A login JWT for the credential's phone, valid 24 hours or until the
    /// credential expires (`POST /telephony_credentials/{id}/token`, which
    /// answers the JWT as plain text).
    pub async fn mint_token(&self, credential_id: &str) -> Result<String, TelnyxError> {
        let token = self
            .send(
                Method::POST,
                &format!("/telephony_credentials/{}/token", segment(credential_id)),
                None,
            )
            .await?
            .text()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        let token = token.trim();
        if token.is_empty() {
            return Err(TelnyxError::Unexpected("empty token"));
        }
        Ok(token.to_string())
    }

    /// Bridges a parked call (Call Control `actions/transfer`).
    pub async fn transfer(
        &self,
        call_control_id: &str,
        transfer: &Transfer,
    ) -> Result<(), TelnyxError> {
        let body = serde_json::to_value(transfer).unwrap_or(Value::Null);
        self.send(
            Method::POST,
            &format!("/calls/{}/actions/transfer", segment(call_control_id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// Hangs a call up (Call Control `actions/hangup`).
    pub async fn hangup(&self, call_control_id: &str, command_id: &str) -> Result<(), TelnyxError> {
        let body = json!({ "command_id": command_id });
        self.send(
            Method::POST,
            &format!("/calls/{}/actions/hangup", segment(call_control_id)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// A recording's download link, length and call (`GET /recordings/{id}`).
    pub async fn get_recording(&self, id: &str) -> Result<Recording, TelnyxError> {
        let answer = self
            .json(Method::GET, &format!("/recordings/{}", segment(id)), None)
            .await?;
        let data = &answer["data"];
        let text = |value: &Value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Ok(Recording {
            mp3_url: text(&data["download_urls"]["mp3"]),
            duration_secs: data["duration_millis"]
                .as_i64()
                .map(|millis| millis.saturating_add(500) / 1000),
            call_control_id: text(&data["call_control_id"]),
            call_leg_id: text(&data["call_leg_id"]),
            call_session_id: text(&data["call_session_id"]),
            connection_id: match &data["connection_id"] {
                Value::Number(number) => Some(number.to_string()),
                other => text(other),
            },
        })
    }

    /// Deletes Telnyx's copy of a recording (`DELETE /recordings/{id}`); a
    /// missing one is fine.
    pub async fn delete_recording(&self, id: &str) -> Result<(), TelnyxError> {
        self.delete(&format!("/recordings/{}", segment(id))).await
    }

    /// Downloads a recording from the short-lived link [`Telnyx::get_recording`]
    /// gave (never a link from a callback); our key is not sent there. Refused
    /// when it is over `max_bytes`, empty, or shorter or longer than its
    /// `Content-Length`.
    pub async fn download(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, TelnyxError> {
        if self.key.is_none() {
            return Err(TelnyxError::NotConfigured);
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(TelnyxError::Unexpected("recording link is not HTTP"));
        }
        let mut response = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(TelnyxError::Status {
                status: response.status().as_u16(),
                code: None,
            });
        }
        let max = u64::try_from(max_bytes).unwrap_or(u64::MAX);
        let declared = response.content_length();
        if declared.is_some_and(|length| length > max) {
            return Err(TelnyxError::Unexpected("recording too large"));
        }
        let mut bytes =
            Vec::with_capacity(usize::try_from(declared.unwrap_or(0).min(max)).unwrap_or_default());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?
        {
            if bytes.len() + chunk.len() > max_bytes {
                return Err(TelnyxError::Unexpected("recording too large"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if declared.is_some_and(|length| u64::try_from(bytes.len()).ok() != Some(length)) {
            return Err(TelnyxError::Unexpected("recording size mismatch"));
        }
        if bytes.is_empty() {
            return Err(TelnyxError::Unexpected("empty recording"));
        }
        Ok(bytes)
    }

    /// Places a call from the TeXML app (`POST /texml/calls/{application_id}`):
    /// Telnyx calls `to` from `from` and, once answered, fetches TeXML from
    /// `url` (POST). The final status goes to `status_callback`. The new
    /// call's `CallSid` when Telnyx says it.
    pub async fn texml_call(
        &self,
        application_id: &str,
        to: &str,
        from: &str,
        url: &str,
        status_callback: Option<&str>,
    ) -> Result<Option<String>, TelnyxError> {
        let mut body = json!({ "To": to, "From": from, "Url": url, "Method": "POST" });
        if let Some(callback) = status_callback {
            body["StatusCallback"] = json!(callback);
            body["StatusCallbackMethod"] = json!("POST");
        }
        let answer = self
            .json(
                Method::POST,
                &format!("/texml/calls/{}", segment(application_id)),
                Some(&body),
            )
            .await?;
        let sid = [
            &answer["data"]["call_sid"],
            &answer["call_sid"],
            &answer["data"]["sid"],
            &answer["sid"],
        ]
        .into_iter()
        .find_map(|value| value.as_str().filter(|sid| !sid.is_empty()))
        .map(str::to_string);
        Ok(sid)
    }

    /// Gives a live TeXML call new instructions from `url` (TeXML REST
    /// "update call" by the call alone, `POST /texml/calls/{call_sid}/update`,
    /// which needs no account id; CamelCase keys as Telnyx asks).
    pub async fn texml_redirect(&self, call_sid: &str, url: &str) -> Result<(), TelnyxError> {
        let body = json!({ "Url": url, "Method": "POST" });
        self.send(
            Method::POST,
            &format!("/texml/calls/{}/update", segment(call_sid)),
            Some(&body),
        )
        .await
        .map(drop)
    }

    /// A Call Control command on a live call (`POST /calls/{id}/actions/{action}`).
    async fn call_action(
        &self,
        call_control_id: &str,
        action: &str,
        body: &Value,
    ) -> Result<(), TelnyxError> {
        self.send(
            Method::POST,
            &format!("/calls/{}/actions/{action}", segment(call_control_id)),
            Some(body),
        )
        .await
        .map(drop)
    }

    /// Starts recording a call, both sides on their own channel, as MP3
    /// (`actions/record_start`); Telnyx sends `call.recording.saved` when
    /// the call ends.
    pub async fn record_start(
        &self,
        call_control_id: &str,
        command_id: &str,
    ) -> Result<(), TelnyxError> {
        let body = json!({ "format": "mp3", "channels": "dual", "command_id": command_id });
        self.call_action(call_control_id, "record_start", &body)
            .await
    }

    /// Pauses (`record_pause`) or resumes (`record_resume`) a call's
    /// recording: one recording, with the paused part left out.
    pub async fn record_pause(
        &self,
        call_control_id: &str,
        pause: bool,
        command_id: &str,
    ) -> Result<(), TelnyxError> {
        let action = if pause {
            "record_pause"
        } else {
            "record_resume"
        };
        self.call_action(
            call_control_id,
            action,
            &json!({ "command_id": command_id }),
        )
        .await
    }

    /// Says `text` to both sides of a call (`actions/speak`), e.g. the
    /// recording notice.
    pub async fn speak(
        &self,
        call_control_id: &str,
        text: &str,
        command_id: &str,
    ) -> Result<(), TelnyxError> {
        let body = json!({
            "payload": text, "voice": "female", "language": "en-US",
            "target_legs": "both", "command_id": command_id,
        });
        self.call_action(call_control_id, "speak", &body).await
    }

    /// Plays the audio at `url` to both sides of a call
    /// (`actions/playback_start`), e.g. the company's recording notice.
    pub async fn play_audio(
        &self,
        call_control_id: &str,
        url: &str,
        command_id: &str,
    ) -> Result<(), TelnyxError> {
        let body = json!({ "audio_url": url, "target_legs": "both", "command_id": command_id });
        self.call_action(call_control_id, "playback_start", &body)
            .await
    }

    /// A form-encoded request (the Twilio-style TeXML REST API).
    async fn send_form(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<reqwest::Response, TelnyxError> {
        let key = self.key.as_deref().ok_or(TelnyxError::NotConfigured)?;
        // Built apart: the serializer must not live across the await.
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            for (name, value) in fields {
                form.append_pair(name, value);
            }
            form.finish()
        };
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(key)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|err| TelnyxError::Request(err.without_url().to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        let code = match &answer["errors"][0]["code"] {
            Value::String(code) => Some(code.clone()),
            Value::Number(code) => Some(code.to_string()),
            _ => None,
        };
        Err(TelnyxError::Status { status, code })
    }

    /// The recordings of a TeXML call: (recording sid, status: `in-progress`,
    /// `paused`, `stopped` or `completed`)
    /// (`GET /texml/Accounts/{account}/Calls/{call}/Recordings.json`).
    pub async fn texml_recordings(
        &self,
        account_sid: &str,
        call_sid: &str,
    ) -> Result<Vec<(String, String)>, TelnyxError> {
        let answer = self
            .json(
                Method::GET,
                &format!(
                    "/texml/Accounts/{}/Calls/{}/Recordings.json",
                    segment(account_sid),
                    segment(call_sid)
                ),
                None,
            )
            .await?;
        Ok(answer["recordings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some((
                    row["sid"].as_str()?.to_string(),
                    row["status"].as_str().unwrap_or_default().to_string(),
                ))
            })
            .collect())
    }

    /// Pauses, resumes or stops a TeXML call's recording (`status`:
    /// `paused`, `in-progress`, `stopped`)
    /// (`POST /texml/Accounts/{account}/Calls/{call}/Recordings/{sid}.json`).
    pub async fn texml_set_recording(
        &self,
        account_sid: &str,
        call_sid: &str,
        recording_sid: &str,
        status: &str,
    ) -> Result<(), TelnyxError> {
        self.send_form(
            &format!(
                "/texml/Accounts/{}/Calls/{}/Recordings/{}.json",
                segment(account_sid),
                segment(call_sid),
                segment(recording_sid)
            ),
            &[("Status", status)],
        )
        .await
        .map(drop)
    }

    /// The numbers assigned to a connection, such as the company's TeXML app
    /// (`GET /phone_numbers?filter[connection_id]=...`, every page).
    pub async fn list_numbers_for_connection(
        &self,
        connection_id: &str,
    ) -> Result<Vec<ConnectionNumber>, TelnyxError> {
        let mut numbers = Vec::new();
        for page in 1..=MAX_NUMBER_PAGES {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("filter[connection_id]", connection_id)
                .append_pair("page[number]", &page.to_string())
                .append_pair("page[size]", &NUMBERS_PAGE_SIZE.to_string())
                .finish();
            let answer = self
                .json(Method::GET, &format!("/phone_numbers?{query}"), None)
                .await?;
            let rows = answer["data"].as_array().cloned().unwrap_or_default();
            for row in &rows {
                let (Some(id), Some(phone_number)) =
                    (row["id"].as_str(), row["phone_number"].as_str())
                else {
                    continue;
                };
                numbers.push(ConnectionNumber {
                    id: id.to_string(),
                    phone_number: phone_number.to_string(),
                });
            }
            let total_pages = answer["meta"]["total_pages"].as_u64().unwrap_or(1);
            if rows.is_empty() || u64::from(page) >= total_pages {
                break;
            }
        }
        Ok(numbers)
    }
}

/// An id as one URL path segment.
fn segment(id: &str) -> String {
    utf8_percent_encode(id, PATH_SEGMENT).to_string()
}

/// ISO 8601 in UTC, to the second, as Telnyx takes dates.
fn telnyx_time(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `text` with XML's special characters escaped; characters XML 1.0
/// cannot hold (control characters other than tab and line breaks) are
/// dropped.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// A TeXML verb or noun: `<Name attr="...">children</Name>`, or
/// `<Name attr="..."/>` without children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verb {
    name: &'static str,
    attrs: Vec<(&'static str, String)>,
    children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Verb(Verb),
    Text(String),
}

impl Verb {
    /// `<name/>`, e.g. `Verb::new("Dial")`.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Adds `name="value"` (escaped), in the order given.
    #[must_use]
    pub fn attr(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.attrs.push((name, value.into()));
        self
    }

    /// Adds `name="value"` when there is a value.
    #[must_use]
    pub fn attr_opt(self, name: &'static str, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.attr(name, value),
            None => self,
        }
    }

    /// Adds text (escaped), e.g. what `<Say>` says or the number `<Number>` dials.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.children.push(Node::Text(text.into()));
        self
    }

    /// Nests a verb or noun, e.g. a `<Sip>` in a `<Dial>`.
    #[must_use]
    pub fn child(mut self, child: Self) -> Self {
        self.children.push(Node::Verb(child));
        self
    }

    fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(self.name);
        for (name, value) in &self.attrs {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&xml_escape(value));
            out.push('"');
        }
        if self.children.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        for child in &self.children {
            match child {
                Node::Verb(verb) => verb.write(out),
                Node::Text(text) => out.push_str(&xml_escape(text)),
            }
        }
        out.push_str("</");
        out.push_str(self.name);
        out.push('>');
    }
}

/// A TeXML document: `<Response>` and its verbs, in order. As an axum
/// response it is `application/xml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    verbs: Vec<Verb>,
}

impl Response {
    pub const fn new() -> Self {
        Self { verbs: Vec::new() }
    }

    /// Appends a verb.
    #[must_use]
    pub fn verb(mut self, verb: Verb) -> Self {
        self.verbs.push(verb);
        self
    }

    /// `<Say>text</Say>`.
    #[must_use]
    pub fn say(self, text: &str) -> Self {
        self.verb(Verb::new("Say").text(text))
    }

    /// `<Hangup/>`.
    #[must_use]
    pub fn hangup(self) -> Self {
        self.verb(Verb::new("Hangup"))
    }

    /// `<Redirect>url</Redirect>`: continue with the TeXML at `url`.
    #[must_use]
    pub fn redirect(self, url: &str) -> Self {
        self.verb(Verb::new("Redirect").text(url))
    }

    /// The document, on one line.
    pub fn to_xml(&self) -> String {
        let mut out = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><Response>"#);
        for verb in &self.verbs {
            verb.write(&mut out);
        }
        out.push_str("</Response>");
        out
    }
}

impl IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        ([(CONTENT_TYPE, "application/xml")], self.to_xml()).into_response()
    }
}

/// Whether a webhook carries Telnyx's signature.
///
/// `signature_b64` (the `telnyx-signature-ed25519` header, base64) must be
/// an Ed25519 signature by `public_key_b64` (`TELNYX_PUBLIC_KEY`, base64) of
/// `"{timestamp}|{body}"`, where `timestamp` is the `telnyx-timestamp`
/// header (Unix seconds) and `body` the raw request body. Refused when the
/// timestamp is more than [`SIGNATURE_WINDOW_SECS`] from `now_unix` (a
/// replay), or anything is malformed.
pub fn verify_signature(
    public_key_b64: &str,
    timestamp: &str,
    body: &[u8],
    signature_b64: &str,
    now_unix: i64,
) -> bool {
    let timestamp = timestamp.trim();
    let Ok(sent_at) = timestamp.parse::<i64>() else {
        return false;
    };
    if now_unix.abs_diff(sent_at) > SIGNATURE_WINDOW_SECS {
        return false;
    }
    let Ok(key) = STANDARD.decode(public_key_b64.trim()) else {
        return false;
    };
    let key = match key.len() {
        32 => key,
        44 if key.starts_with(&ED25519_SPKI_PREFIX) => key[12..].to_vec(),
        _ => return false,
    };
    let Ok(signature) = STANDARD.decode(signature_b64.trim()) else {
        return false;
    };
    let mut message = Vec::with_capacity(timestamp.len() + 1 + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.push(b'|');
    message.extend_from_slice(body);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
        .verify(&message, &signature)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Request;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::sync::Mutex;

    #[test]
    fn texml_escapes_every_attribute_and_text() {
        let xml = Response::new()
            .say("Tom & Jerry's <\"cafe\">")
            .verb(
                Verb::new("Dial")
                    .attr("answerOnBridge", "true")
                    .attr("action", "https://api.test/v1/x?call=1&kind=call")
                    .child(
                        Verb::new("Sip")
                            .attr("statusCallback", "https://api.test/leg?call=1&device=2")
                            .text("sip:gencred1@sip.telnyx.com"),
                    ),
            )
            .verb(
                Verb::new("Record")
                    .attr_opt("maxLength", Some("120"))
                    .attr_opt("playBeep", None::<&str>),
            )
            .hangup()
            .to_xml();
        assert_eq!(
            xml,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response>\
             <Say>Tom &amp; Jerry&apos;s &lt;&quot;cafe&quot;&gt;</Say>\
             <Dial answerOnBridge=\"true\" action=\"https://api.test/v1/x?call=1&amp;kind=call\">\
             <Sip statusCallback=\"https://api.test/leg?call=1&amp;device=2\">sip:gencred1@sip.telnyx.com</Sip>\
             </Dial><Record maxLength=\"120\"/><Hangup/></Response>"
        );
        assert_eq!(
            Response::new()
                .redirect("https://bin.test/a?b=1&c=2")
                .to_xml(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response>\
             <Redirect>https://bin.test/a?b=1&amp;c=2</Redirect></Response>"
        );
    }

    #[test]
    fn xml_drops_characters_it_cannot_hold() {
        assert_eq!(xml_escape("a\u{0}b\u{1b}c\td\ne"), "abc\td\ne");
        assert_eq!(xml_escape("Zoë Ünicode"), "Zoë Ünicode");
        assert_eq!(
            Response::new()
                .verb(Verb::new("Say").attr("voice", "x\"y").text(""))
                .to_xml(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Say voice=\"x&quot;y\"></Say></Response>"
        );
    }

    fn key_pair() -> (Ed25519KeyPair, String) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public = STANDARD.encode(pair.public_key().as_ref());
        (pair, public)
    }

    fn sign(pair: &Ed25519KeyPair, timestamp: &str, body: &str) -> String {
        STANDARD.encode(pair.sign(format!("{timestamp}|{body}").as_bytes()).as_ref())
    }

    #[test]
    fn signatures_by_the_key_are_accepted() {
        let (pair, public) = key_pair();
        let body = r#"{"data":{"event_type":"call.initiated"}}"#;
        let signature = sign(&pair, "1760700000", body);
        assert!(verify_signature(
            &public,
            "1760700000",
            body.as_bytes(),
            &signature,
            1_760_700_010
        ));
        assert!(verify_signature(
            &public,
            " 1760700000 ",
            body.as_bytes(),
            &signature,
            1_760_700_000
        ));
        // The key as SubjectPublicKeyInfo (DER) works too.
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        assert!(verify_signature(
            &STANDARD.encode(spki),
            "1760700000",
            body.as_bytes(),
            &signature,
            1_760_700_000
        ));
    }

    #[test]
    fn forged_or_mangled_signatures_are_refused() {
        let (pair, public) = key_pair();
        let (other, other_public) = key_pair();
        let body = r#"{"data":{"id":"1"}}"#;
        let signature = sign(&pair, "1760700000", body);
        let now = 1_760_700_000;
        // Another body, timestamp or key.
        assert!(!verify_signature(
            &public,
            "1760700000",
            br#"{"data":{"id":"2"}}"#,
            &signature,
            now
        ));
        assert!(!verify_signature(
            &public,
            "1760700001",
            body.as_bytes(),
            &signature,
            now
        ));
        assert!(!verify_signature(
            &other_public,
            "1760700000",
            body.as_bytes(),
            &signature,
            now
        ));
        assert!(!verify_signature(
            &public,
            "1760700000",
            body.as_bytes(),
            &sign(&other, "1760700000", body),
            now
        ));
        // Malformed parts.
        assert!(!verify_signature(
            &public,
            "soon",
            body.as_bytes(),
            &signature,
            now
        ));
        assert!(!verify_signature(
            &public,
            "1760700000",
            body.as_bytes(),
            "not base64!",
            now
        ));
        assert!(!verify_signature(
            "short",
            "1760700000",
            body.as_bytes(),
            &signature,
            now
        ));
        assert!(!verify_signature(
            &public,
            "1760700000",
            body.as_bytes(),
            "",
            now
        ));
    }

    #[test]
    fn signatures_outside_five_minutes_are_replays() {
        let (pair, public) = key_pair();
        let body = "{}";
        let signature = sign(&pair, "1760700000", body);
        let check =
            |now: i64| verify_signature(&public, "1760700000", body.as_bytes(), &signature, now);
        assert!(check(1_760_700_300));
        assert!(check(1_760_699_700));
        assert!(!check(1_760_700_301));
        assert!(!check(1_760_699_699));
    }

    /// One request the stub received.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        uri: String,
        auth: Option<String>,
        body: Value,
    }

    /// A stand-in for Telnyx on 127.0.0.1 that records every request.
    async fn stub() -> (Telnyx, Arc<Mutex<Vec<Seen>>>) {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let log = Arc::clone(&seen);
        let router = axum::Router::new().fallback(move |request: Request| {
            let log = Arc::clone(&log);
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                let uri = parts.uri.to_string();
                log.lock().unwrap().push(Seen {
                    method: parts.method.to_string(),
                    uri: uri.clone(),
                    auth: parts
                        .headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string),
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                });
                answer(parts.method.as_str(), &uri)
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (
            Telnyx::new(Some(" test-key "), &format!("http://{address}/")),
            seen,
        )
    }

    fn answer(method: &str, uri: &str) -> axum::response::Response {
        use axum::http::StatusCode;
        let ok = |body: Value| (StatusCode::OK, axum::Json(body)).into_response();
        match (method, uri) {
            ("POST", "/telephony_credentials") => ok(json!({ "data": {
                "id": "cred-1", "record_type": "credential", "sip_username": "gencredabc",
                "sip_password": "secret", "expired": false } })),
            ("POST", "/telephony_credentials/cred-1/token") => (
                StatusCode::OK,
                [(CONTENT_TYPE, "text/plain")],
                "eyJhbGciOi.payload.sig\n",
            )
                .into_response(),
            ("DELETE", "/telephony_credentials/gone") => (
                StatusCode::NOT_FOUND,
                axum::Json(
                    json!({ "errors": [{ "code": "10005", "title": "Resource not found" }] }),
                ),
            )
                .into_response(),
            ("POST", "/calls/v3:ended/actions/hangup") => (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(
                    json!({ "errors": [{ "code": "90018", "title": "Call has already ended",
                    "detail": "This call is no longer active" }] }),
                ),
            )
                .into_response(),
            ("GET", "/recordings/rec-1") => ok(json!({ "data": {
                "id": "rec-1", "duration_millis": 61_500, "status": "completed",
                "call_control_id": "v3:leg-1", "call_leg_id": "leg-uuid-1",
                "call_session_id": "session-1", "connection_id": 1_684_641_123_236_054_244_u64,
                "download_urls": { "mp3": "https://s3.test/rec-1.mp3", "wav": "https://s3.test/rec-1.wav" } } })),
            ("POST", "/texml/calls/app-1") => ok(json!({ "data": {
                "call_sid": "v3:texml-sid", "from": "+13175550100", "to": "+13175550142", "status": "queued" } })),
            ("GET", uri) if uri.starts_with("/phone_numbers?") => {
                let page2 = uri.contains("page%5Bnumber%5D=2");
                let data = if page2 {
                    json!([{ "id": "n-3", "phone_number": "+13175550103" }])
                } else {
                    json!([{ "id": "n-1", "phone_number": "+13175550101" },
                           { "id": "n-2", "phone_number": "+13175550102" },
                           { "id": "n-x" }])
                };
                ok(
                    json!({ "data": data, "meta": { "total_pages": 2, "page_number": if page2 { 2 } else { 1 } } }),
                )
            }
            _ => ok(json!({ "data": {} })),
        }
    }

    #[tokio::test]
    async fn credentials_are_created_extended_minted_and_deleted() {
        let (telnyx, seen) = stub().await;
        let expires = DateTime::parse_from_rfc3339("2026-11-16T10:00:00.250Z")
            .unwrap()
            .with_timezone(&Utc);
        let credential = telnyx
            .create_credential("conn-1", "phone-7", "company-100", expires)
            .await
            .unwrap();
        assert_eq!(
            credential,
            Credential {
                id: "cred-1".into(),
                sip_username: "gencredabc".into()
            }
        );
        telnyx
            .set_credential_expiry("cred-1", expires)
            .await
            .unwrap();
        assert_eq!(
            telnyx.mint_token("cred-1").await.unwrap(),
            "eyJhbGciOi.payload.sig"
        );
        telnyx.delete_credential("cred-1").await.unwrap();
        // Already gone: nothing left to do.
        telnyx.delete_credential("gone").await.unwrap();
        let seen = seen.lock().unwrap().clone();
        let calls: Vec<(&str, &str, &Value)> = seen
            .iter()
            .map(|s| (s.method.as_str(), s.uri.as_str(), &s.body))
            .collect();
        let created = json!({
            "connection_id": "conn-1", "name": "phone-7", "tag": "company-100",
            "expires_at": "2026-11-16T10:00:00Z" });
        let extended = json!({ "expires_at": "2026-11-16T10:00:00Z" });
        assert_eq!(
            calls,
            [
                ("POST", "/telephony_credentials", &created),
                ("PATCH", "/telephony_credentials/cred-1", &extended),
                ("POST", "/telephony_credentials/cred-1/token", &Value::Null),
                ("DELETE", "/telephony_credentials/cred-1", &Value::Null),
                ("DELETE", "/telephony_credentials/gone", &Value::Null),
            ]
        );
        assert!(
            seen.iter()
                .all(|s| s.auth.as_deref() == Some("Bearer test-key"))
        );
    }

    #[tokio::test]
    async fn call_control_commands_have_telnyx_shapes() {
        let (telnyx, seen) = stub().await;
        let transfer = Transfer {
            to: "+13175550142".into(),
            from: Some("+13175550100".into()),
            record: Some("record-from-answer".into()),
            record_channels: Some("dual".into()),
            record_format: Some("mp3".into()),
            timeout_secs: Some(60),
            webhook_url: Some("https://api.test/v1/webhooks/telnyx/tok/outbound".into()),
            command_id: Some("approve-42".into()),
            ..Transfer::default()
        };
        telnyx.transfer("v3:abc-1_x", &transfer).await.unwrap();
        telnyx.hangup("v3:abc/1", "hangup-42").await.unwrap();
        let err = telnyx.hangup("v3:ended", "hangup-43").await.unwrap_err();
        assert_eq!(err.status(), Some(422));
        assert_eq!(err.code(), Some("90018"));
        // Telnyx's detail text is not in the error.
        assert_eq!(err.to_string(), "Telnyx answered 422");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].uri, "/calls/v3:abc-1_x/actions/transfer");
        assert_eq!(
            seen[0].body,
            json!({
                "to": "+13175550142", "from": "+13175550100", "record": "record-from-answer",
                "record_channels": "dual", "record_format": "mp3", "timeout_secs": 60,
                "webhook_url": "https://api.test/v1/webhooks/telnyx/tok/outbound",
                "command_id": "approve-42"
            })
        );
        // Ids are one path segment, whatever they hold.
        assert_eq!(seen[1].uri, "/calls/v3:abc%2F1/actions/hangup");
        assert_eq!(seen[1].body, json!({ "command_id": "hangup-42" }));
    }

    #[tokio::test]
    async fn recordings_texml_calls_and_numbers() {
        let (telnyx, seen) = stub().await;
        let recording = telnyx.get_recording("rec-1").await.unwrap();
        assert_eq!(
            recording,
            Recording {
                mp3_url: Some("https://s3.test/rec-1.mp3".into()),
                duration_secs: Some(62),
                call_control_id: Some("v3:leg-1".into()),
                call_leg_id: Some("leg-uuid-1".into()),
                call_session_id: Some("session-1".into()),
                connection_id: Some("1684641123236054244".into()),
            }
        );
        assert_eq!(
            recording.call_ids().collect::<Vec<_>>(),
            ["v3:leg-1", "leg-uuid-1", "session-1"]
        );
        telnyx.delete_recording("rec-1").await.unwrap();
        let sid = telnyx
            .texml_call(
                "app-1",
                "+13175550142",
                "+13175550100",
                "https://api.test/v1/webhooks/telnyx/tok/via-cell?call=7",
                Some("https://api.test/v1/webhooks/telnyx/tok/call-status"),
            )
            .await
            .unwrap();
        assert_eq!(sid.as_deref(), Some("v3:texml-sid"));
        telnyx
            .texml_redirect(
                "v3:texml-sid",
                "https://api.test/dial-result?call=7&declined=1",
            )
            .await
            .unwrap();
        let numbers = telnyx.list_numbers_for_connection("app-1").await.unwrap();
        assert_eq!(
            numbers
                .iter()
                .map(|n| (n.id.as_str(), n.phone_number.as_str()))
                .collect::<Vec<_>>(),
            [
                ("n-1", "+13175550101"),
                ("n-2", "+13175550102"),
                ("n-3", "+13175550103")
            ]
        );
        let seen = seen.lock().unwrap().clone();
        let calls: Vec<(&str, &str)> = seen
            .iter()
            .map(|s| (s.method.as_str(), s.uri.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("GET", "/recordings/rec-1"),
                ("DELETE", "/recordings/rec-1"),
                ("POST", "/texml/calls/app-1"),
                ("POST", "/texml/calls/v3:texml-sid/update"),
                (
                    "GET",
                    "/phone_numbers?filter%5Bconnection_id%5D=app-1&page%5Bnumber%5D=1&page%5Bsize%5D=250"
                ),
                (
                    "GET",
                    "/phone_numbers?filter%5Bconnection_id%5D=app-1&page%5Bnumber%5D=2&page%5Bsize%5D=250"
                ),
            ]
        );
        assert_eq!(
            seen[2].body,
            json!({
                "To": "+13175550142", "From": "+13175550100",
                "Url": "https://api.test/v1/webhooks/telnyx/tok/via-cell?call=7", "Method": "POST",
                "StatusCallback": "https://api.test/v1/webhooks/telnyx/tok/call-status",
                "StatusCallbackMethod": "POST"
            })
        );
        assert_eq!(
            seen[3].body,
            json!({ "Url": "https://api.test/dial-result?call=7&declined=1", "Method": "POST" })
        );
    }

    #[tokio::test]
    async fn without_a_key_nothing_is_sent() {
        let telnyx = Telnyx::new(Some("  "), "http://127.0.0.1:9");
        assert!(!telnyx.is_enabled());
        assert!(matches!(
            telnyx.delete_credential("cred-1").await,
            Err(TelnyxError::NotConfigured)
        ));
        assert!(!Telnyx::disabled().is_enabled());
    }
}
