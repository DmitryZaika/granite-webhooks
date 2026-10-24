//! Shared fixtures of the Granite Phone tests (`tests/phone_*.rs`): device
//! sessions and phones, a company's Telnyx setup, numbers and calls, and one
//! stand-in server for Telnyx, FCM, the webhooks Lambda, AssemblyAI and
//! OpenAI.
//!
//! Fixture ids: device sessions 950000 + n, phone devices 960000 + n (token
//! `token(n)`), numbers 9700+, calls 980000+, customers 930000+.
#![allow(dead_code)]

use crate::common::{ALLOWED_ORIGIN, SECRET, WithSession, sessions};
use api::ai::Ai;
use api::app;
use api::auth::device::token_hash;
use api::phone::PhoneConfig;
use api::push::{Push, Source};
use api::state::AppState;
use api::storage::Storage;
use api::telnyx::Telnyx;
use api::transcribe::Transcriber;
use api::webhooks_client::WebhooksClient;
use axum::Json;
use axum::Router;
use axum::extract::Request;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_test::TestServer;
use serde_json::{Value, json};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const HEADER: &str = "x-granite-device";
pub const TELNYX_KEY: &str = "test-telnyx-key";
/// `PHONE_PUBLIC_API_URL` in the tests.
pub const PUBLIC_API: &str = "https://api.test";
/// The app's URL (`APP_URL`) in the tests.
pub const APP_URL: &str = "https://crm.test";
/// The login JWT the stand-in mints: it expires at 2030-03-17T17:46:40Z.
pub const JWT: &str = "eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjE5MDAwMDAwMDB9.c2ln";
pub const JWT_EXPIRES: &str = "2030-03-17T17:46:40.000Z";
/// A number Telnyx refuses to call.
pub const REFUSED_NUMBER: &str = "+13175550999";
/// A push token whose app is gone (FCM answers `UNREGISTERED`).
pub const GONE_PUSH: &str = "push-gone";
/// The `time-triggered` Lambda's key (`LAMBDA_KEY`) in the tests.
pub const JOB_KEY: &str = "job-secret";

/// A test-only RSA key (the one of `api/src/push.rs`'s tests).
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCvYcSvoLqURAdp
OGTc6bNUl3SWYX7d0+ngHbyG3w2HDEVKCppN0Ik83/9L8rXjTC0qhSsURflxgny5
HohF4YbYNuKtnaIt4wNJuXJY1fm9jaB9/mVQwm6tEksshgvBy10F1qAP61adUHSq
u/ELDJpYgriLcjvesQg7s81dO3PZl+lbvptfNa8NTo4F6tskYfTg5cabIMCAZNJ8
vp7bJZdmbELH4NehPiK3/ZSjxfQjmrHez+mvWukmTEt0F9T4gaXWR7B++jzSa/bA
zHiHLZ0quukocEkiLF5ezUMDcHzvtD8hmLJ7BLNSUqS8jaG9ld8L6rRbFe3zudxB
9XIBvEbjAgMBAAECggEAM618Vj/nGm8dO+zpE4stU1y5duJNjrG1tVstxRQOmV+N
Py72nr024yAhytA+tjYpDOFmezfPM5o+hBGyF91HbYhmrL/JHPpKarvhLLoIN8l2
pAmE8xIHqKm+SuVIq5bv1xuR2H8A66PcbL6qXRYemLwUAsrrR0gk4+qsNa+iCFhT
iKqGWlyHKP49tgkQO1Vtqp84QpsU5qAoU9TeqQysEddNhhWF7bJFGpkMOrSYWppi
aIFD8XBQi/KMKLo2jBt/KuzNZ3vJGBNX9BLg7ByoTqlD74vn5vLk5NUwCL1b2S0f
SNkrDZIgPwSug9fGsbW4YnYloOr/vLt7uGP1AZZ/AQKBgQD3KW72F+TW21A8Hc06
2cNNvq29AD/rqnZOh8cxNQkMRBS151PfqBCqZBr/7Hzk/+GGXErxMeVIsF8FhiKU
tIXlQqSjtIDFNWTH0S7egzdDIFHtpJBIKqC5DpsdZdpCSqwZOz7d9RJ8VQqsaFsl
p6edzYGkZiR+g1vY1For6aw1YwKBgQC1pz9khWlm+emM9iu6S7zgzLOUBTtvJb/d
o3maPtZjMrxPQUHy6Ai5ocQh8VoepbIGQo9pNzV27+1MWvnjIg8XEi+KP5us0919
m+N6StaR4wsamsV8BHyZ+39vaqLa26L0Hw6ZiM6mRwQMhm+rpOzVy78H4fOCB0Pn
q5lfcmMggQKBgDjfJ9EonldyD3xFvzuTcAUrov6iWBL6Hk7jn0oLUhGoeLuChyIn
oe4TKsNIv1RSLKqmAfiN6pxXOQYkX0CoaUbfltK0bgzyxEXWNFqvBsBDZ5avDyQL
6v+qBLc3AInQdNk5cLXR9nb1eodBW/fGJCODbhlOB4e04iV6d2UzV5TRAoGBALT2
kGiLn50+ijQraRSGLvuj8aIcNEFd0e3y1UmcH+diu8YYtKJQFaEYTlOiJK0zVE54
MHYY0eJS0oumdhqX64CIVWPcSjG6PoTLr6o8s9VR4dX3KZWKwraDnUhZazOsaoHY
DqtVZsivJpFzu/u6qVWzS/OGXRfENRmuEXTMd4QBAoGBANL8/58xTPS91KkZ35sl
kqLLg5a9wcTTvJd8ikHcda7qGXuPyZdRYW8EPjr1yEnBApW4MPGyQ/6qybIhynsx
AlZwzd3vIQiGi4f8RV85Z65yMpMjIyDi/8A++W3H+bHfav09g6oQlJl+rcd6jBP+
zVwmY+zvp0zIX68RsO90kuxb
-----END PRIVATE KEY-----";

/// A well-formed device token for fixture `n` (43 characters).
pub fn token(n: u32) -> String {
    format!("{n:0>43}")
}

/// The call key of fixture phone `n` (device session `950000 + n`), as
/// sign-in hands it out: HMAC-SHA256 keyed with the tests' `SESSION_SECRET`
/// over `granite-phone-call-key:{session id}`, made apart from the API's
/// own derivation.
pub fn call_key(n: u32) -> Vec<u8> {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("granite-phone-call-key:{}", 950_000 + n).as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The `X-Granite-Call-Auth` phone `n` makes with its call key
/// ([`call_key`]) for a call of device `device` to `to` with caller ID
/// `caller` at `time`, as the app's `CallEngine.callAuth` does: HMAC-SHA256
/// keyed with the key's 32 bytes over `v1|device|to|caller|time`.
pub fn call_auth(n: u32, device: &str, to: &str, caller: &str, time: i64) -> String {
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&call_key(n)).unwrap();
    mac.update(format!("v1|{device}|{to}|{caller}|{time}").as_bytes());
    format!(
        "v1.{time}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

/// The webhook token of a company's fixture Telnyx setup (32 characters).
pub fn webhook_token(company_id: i32) -> String {
    format!("tok{company_id:0>29}")
}

/// The root of a company's Telnyx callbacks.
pub fn webhook_base(company_id: i32) -> String {
    format!(
        "{PUBLIC_API}/v1/webhooks/telnyx/{}",
        webhook_token(company_id)
    )
}

/// Runs SQL fixtures (one statement).
pub async fn sql(pool: &MySqlPool, statement: &str) {
    sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
        .execute(pool)
        .await
        .unwrap_or_else(|err| panic!("{statement}: {err}"));
}

/// A live device session `950000 + n` of `user_id` in `company_id`, with the
/// token `token(n)`.
pub async fn add_session(pool: &MySqlPool, n: u32, user_id: i32, company_id: i32) -> i64 {
    let id = 950_000 + i64::from(n);
    sqlx::query(
        "INSERT INTO device_sessions
           (id, user_id, company_id, token_hash, device_name, platform, app_version,
            created_at, last_seen_at, expires_at)
         VALUES (?, ?, ?, ?, 'Test phone', 'android', '1.0.0', UTC_TIMESTAMP() - INTERVAL 1 DAY,
                 UTC_TIMESTAMP() - INTERVAL 1 MINUTE, UTC_TIMESTAMP() + INTERVAL 11 MONTH)",
    )
    .bind(id)
    .bind(user_id)
    .bind(company_id)
    .bind(token_hash(&token(n)))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Signs device session `950000 + n` out (its token stops working).
pub async fn revoke_session(pool: &MySqlPool, n: u32) {
    sql(
        pool,
        &format!(
            "UPDATE device_sessions SET revoked_at = UTC_TIMESTAMP() WHERE id = {}",
            950_000 + i64::from(n)
        ),
    )
    .await;
}

/// Each route answers 401 without a device token, with only the CRM cookie
/// of an employee, and with `token(revoked)` (a revoked session).
pub async fn assert_device_only(server: &TestServer, revoked: u32, routes: &[(Method, String)]) {
    for (method, path) in routes {
        let requests = [
            server.method(method.clone(), path),
            server
                .method(method.clone(), path)
                .with_session(sessions::REP),
            server
                .method(method.clone(), path)
                .add_header(HEADER, token(revoked)),
        ];
        for request in requests {
            let response = request.json(&json!({})).await;
            assert_eq!(
                response.status_code(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
        }
    }
}

/// A signed-in, registered phone `n`: device session `950000 + n` and phone
/// device `960000 + n` with credential `cred-{n}`, SIP user `gencred{n}` and
/// push token `push-{n}`. Returns the phone device id.
pub async fn add_phone(pool: &MySqlPool, n: u32, user_id: i32, company_id: i32) -> i64 {
    let session = add_session(pool, n, user_id, company_id).await;
    let device = 960_000 + i64::from(n);
    sqlx::query(
        "INSERT INTO phone_devices
           (id, device_session_id, user_id, company_id, platform, device_name, app_version,
            telnyx_credential_id, sip_username, credential_expires_at, push_token, created_at)
         VALUES (?, ?, ?, ?, 'android', 'Test phone', '1.0.0', ?, ?,
                 UTC_TIMESTAMP() + INTERVAL 20 DAY, ?, '2026-10-01 09:00:00')",
    )
    .bind(device)
    .bind(session)
    .bind(user_id)
    .bind(company_id)
    .bind(format!("cred-{n}"))
    .bind(format!("gencred{n}"))
    .bind(format!("push-{n}"))
    .execute(pool)
    .await
    .unwrap();
    device
}

/// Calling switched on for the company: enabled, credential connection
/// `conn-{id}`, TeXML app `app-{id}`, account `acct-{id}`; `extra` sets
/// more columns (`recording_mode = 'both'`).
pub async fn enable_calling(pool: &MySqlPool, company_id: i32, extra: &str) {
    sqlx::query(
        "INSERT INTO telnyx_company_config
           (company_id, enabled, webhook_token, credential_connection_id, texml_app_id,
            telnyx_account_id)
         VALUES (?, 1, ?, ?, ?, ?)",
    )
    .bind(company_id)
    .bind(webhook_token(company_id))
    .bind(format!("conn-{company_id}"))
    .bind(format!("app-{company_id}"))
    .bind(format!("acct-{company_id}"))
    .execute(pool)
    .await
    .unwrap();
    if !extra.is_empty() {
        sql(
            pool,
            &format!("UPDATE telnyx_company_config SET {extra} WHERE company_id = {company_id}"),
        )
        .await;
    }
}

/// A company number.
pub async fn add_number(
    pool: &MySqlPool,
    id: i32,
    company_id: i32,
    e164: &str,
    kind: &str,
    label: &str,
) {
    sqlx::query(
        "INSERT INTO telnyx_numbers (id, company_id, e164, last10, label, kind)
         VALUES (?, ?, ?, RIGHT(?, 10), ?, ?)",
    )
    .bind(id)
    .bind(company_id)
    .bind(e164)
    .bind(e164)
    .bind(label)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
}

/// Gives a number to a user.
pub async fn assign(
    pool: &MySqlPool,
    number_id: i32,
    user_id: i32,
    rings: bool,
    can_call_from: bool,
) {
    sqlx::query(
        "INSERT INTO telnyx_number_users (number_id, user_id, rings, can_call_from)
         VALUES (?, ?, ?, ?)",
    )
    .bind(number_id)
    .bind(user_id)
    .bind(rings)
    .bind(can_call_from)
    .execute(pool)
    .await
    .unwrap();
}

/// A call-log row: `company_id`, the handling user, `direction`, `status`,
/// the other number (E.164) and when it started (UTC); `extra` sets more
/// columns.
#[allow(clippy::too_many_arguments)]
pub async fn add_call(
    pool: &MySqlPool,
    id: i64,
    company_id: i32,
    user_id: Option<i32>,
    direction: &str,
    status: &str,
    other: &str,
    started_at: &str,
    extra: &str,
) {
    sqlx::query(
        "INSERT INTO phone_calls
           (id, company_id, user_id, direction, status, other_number, other_last10, started_at)
         VALUES (?, ?, ?, ?, ?, ?, RIGHT(?, 10), ?)",
    )
    .bind(id)
    .bind(company_id)
    .bind(user_id)
    .bind(direction)
    .bind(status)
    .bind(other)
    .bind(other)
    .bind(started_at)
    .execute(pool)
    .await
    .unwrap();
    if !extra.is_empty() {
        sql(
            pool,
            &format!("UPDATE phone_calls SET {extra} WHERE id = {id}"),
        )
        .await;
    }
}

/// A verified cell (and other settings) for a user in a company.
pub async fn set_settings(pool: &MySqlPool, user_id: i32, company_id: i32, columns: &str) {
    sql(
        pool,
        &format!(
            "INSERT INTO phone_user_settings (user_id, company_id) VALUES ({user_id}, {company_id})"
        ),
    )
    .await;
    if !columns.is_empty() {
        sql(
            pool,
            &format!("UPDATE phone_user_settings SET {columns} WHERE user_id = {user_id}"),
        )
        .await;
    }
}

/// One request Telnyx's stand-in received: method, path with query, and
/// the JSON body (null when none).
pub type Seen = (String, String, Value);

/// What the stand-ins received, and switches for them.
#[derive(Clone, Default)]
pub struct Stubs {
    telnyx: Arc<Mutex<Vec<Seen>>>,
    pushes: Arc<Mutex<Vec<Value>>>,
    emails: Arc<Mutex<Vec<Value>>>,
    /// While set, Telnyx answers 500 to everything.
    pub telnyx_down: Arc<AtomicBool>,
    /// While set, Telnyx answers 500 to deletes (credentials, recordings).
    pub fail_deletes: Arc<AtomicBool>,
    /// While set, FCM takes 5 seconds to answer.
    pub slow_pushes: Arc<AtomicBool>,
    credentials: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    /// The stand-in's own root (`http://127.0.0.1:port`).
    base: Arc<Mutex<String>>,
    /// Recordings Telnyx knows: id → (MP3 link, length in milliseconds, the
    /// call ids Telnyx gives for it).
    recordings: Arc<Mutex<HashMap<String, (String, i64, Value)>>>,
    /// Files served at `/files/{name}` (the recordings' MP3s).
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// Numbers Telnyx lists for any connection: (Telnyx id, E.164).
    phone_numbers: Arc<Mutex<Vec<(String, String)>>>,
    /// AssemblyAI's finished transcripts, by the audio uploaded.
    transcripts: Arc<Mutex<HashMap<Vec<u8>, Value>>>,
    /// Audio uploaded to AssemblyAI, in order.
    uploads: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Transcript requests sent to AssemblyAI.
    transcript_requests: Arc<Mutex<Vec<Value>>>,
    /// Chat requests sent to OpenAI.
    chats: Arc<Mutex<Vec<Value>>>,
    /// What OpenAI answers.
    chat_reply: Arc<Mutex<String>>,
    /// What OpenAI answers to the voicemail prompt, when set.
    voicemail_reply: Arc<Mutex<Option<String>>>,
    /// While set, OpenAI answers 500.
    pub chat_down: Arc<AtomicBool>,
    /// Names Telnyx says are in use (`10015`) when a resource is made.
    taken_names: Arc<Mutex<Vec<String>>>,
    /// Resources made so far (billing groups, profiles, apps, connections).
    made: Arc<AtomicUsize>,
}

impl Stubs {
    /// Telnyx requests so far.
    pub fn telnyx(&self) -> Vec<Seen> {
        self.telnyx.lock().unwrap().clone()
    }

    /// `METHOD path` of each Telnyx request so far.
    pub fn telnyx_paths(&self) -> Vec<String> {
        self.telnyx()
            .into_iter()
            .map(|(method, path, _)| format!("{method} {path}"))
            .collect()
    }

    /// Forgets the Telnyx requests so far.
    pub fn clear_telnyx(&self) {
        self.telnyx.lock().unwrap().clear();
    }

    /// FCM `message`s sent so far.
    pub fn pushes(&self) -> Vec<Value> {
        self.pushes.lock().unwrap().clone()
    }

    /// Emails sent through the webhooks Lambda so far.
    pub fn emails(&self) -> Vec<Value> {
        self.emails.lock().unwrap().clone()
    }

    /// The stand-in's root, e.g. `http://127.0.0.1:61234`.
    pub fn base(&self) -> String {
        self.base.lock().unwrap().clone()
    }

    /// A recording Telnyx has of the call leg `call_control_id` (a TeXML
    /// `CallSid`): `GET /recordings/{id}` answers its MP3 link, length and
    /// call.
    pub fn add_recording(
        &self,
        id: &str,
        mp3_url: &str,
        duration_millis: i64,
        call_control_id: &str,
    ) {
        self.add_recording_of(
            id,
            mp3_url,
            duration_millis,
            json!({ "call_control_id": call_control_id, "call_leg_id": format!("leg-of-{call_control_id}") }),
        );
    }

    /// A recording Telnyx has, with these call ids (`call_control_id`,
    /// `call_leg_id`, `call_session_id`, `connection_id`).
    pub fn add_recording_of(&self, id: &str, mp3_url: &str, duration_millis: i64, call: Value) {
        self.recordings
            .lock()
            .unwrap()
            .insert(id.to_string(), (mp3_url.to_string(), duration_millis, call));
    }

    /// Serves `bytes` at `/files/{name}`; the file's link.
    pub fn add_file(&self, name: &str, bytes: &[u8]) -> String {
        self.files
            .lock()
            .unwrap()
            .insert(name.to_string(), bytes.to_vec());
        format!("{}/files/{name}", self.base())
    }

    /// A number on Telnyx: `GET /phone_numbers` lists it (for any
    /// connection).
    pub fn add_telnyx_number(&self, id: &str, e164: &str) {
        self.phone_numbers
            .lock()
            .unwrap()
            .push((id.to_string(), e164.to_string()));
    }

    /// Telnyx refuses to make a resource named `name` (`10015`, in use).
    pub fn take_name(&self, name: &str) {
        self.taken_names.lock().unwrap().push(name.to_string());
    }

    /// AssemblyAI transcribes `audio` as `text`, with these speaker turns
    /// (`(speaker, text)`) when speaker labels are asked for.
    pub fn add_transcript(&self, audio: &[u8], text: &str, turns: &[(&str, &str)]) {
        let utterances: Vec<Value> = turns
            .iter()
            .map(|(speaker, text)| json!({ "speaker": speaker, "text": text }))
            .collect();
        self.transcripts.lock().unwrap().insert(
            audio.to_vec(),
            json!({ "status": "completed", "text": text, "utterances": utterances,
                    "audio_duration": 61.0 }),
        );
    }

    /// Audio uploaded to AssemblyAI so far.
    pub fn uploads(&self) -> Vec<Vec<u8>> {
        self.uploads.lock().unwrap().clone()
    }

    /// Transcript requests sent to AssemblyAI so far.
    pub fn transcript_requests(&self) -> Vec<Value> {
        self.transcript_requests.lock().unwrap().clone()
    }

    /// Chat requests sent to OpenAI so far.
    pub fn chats(&self) -> Vec<Value> {
        self.chats.lock().unwrap().clone()
    }

    /// What OpenAI answers from now on.
    pub fn set_chat_reply(&self, reply: &str) {
        *self.chat_reply.lock().unwrap() = reply.to_string();
    }

    /// What OpenAI answers to the voicemail prompt from now on (else the
    /// chat reply).
    pub fn set_voicemail_reply(&self, reply: &str) {
        *self.voicemail_reply.lock().unwrap() = Some(reply.to_string());
    }

    /// AssemblyAI's upload, transcript request and transcript.
    fn assemblyai_answer(&self, method: &str, path: &str, bytes: &[u8], body: &Value) -> Response {
        match (method, path) {
            ("POST", "/v2/upload") => {
                let mut uploads = self.uploads.lock().unwrap();
                uploads.push(bytes.to_vec());
                let n = uploads.len() - 1;
                Json(json!({ "upload_url": format!("https://aai.test/upload/{n}") }))
                    .into_response()
            }
            ("POST", "/v2/transcript") => {
                self.transcript_requests.lock().unwrap().push(body.clone());
                let n = body["audio_url"]
                    .as_str()
                    .and_then(|url| url.rsplit('/').next())
                    .unwrap_or_default()
                    .to_string();
                Json(json!({ "id": format!("t{n}") })).into_response()
            }
            ("GET", _) if path.starts_with("/v2/transcript/t") => {
                let n: usize = path
                    .trim_start_matches("/v2/transcript/t")
                    .parse()
                    .unwrap_or(usize::MAX);
                let audio = self.uploads.lock().unwrap().get(n).cloned();
                let known =
                    audio.and_then(|audio| self.transcripts.lock().unwrap().get(&audio).cloned());
                Json(
                    known.unwrap_or_else(|| json!({ "status": "error", "error": "unknown audio" })),
                )
                .into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    fn telnyx_answer(&self, method: &str, path: &str, body: &Value) -> Response {
        let ok = |body: Value| (StatusCode::OK, Json(body)).into_response();
        let not_found = || {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "errors": [{ "code": "10005", "title": "Resource not found" }] })),
            )
                .into_response()
        };
        // What "Connect Telnyx" makes: billing groups, voice profiles (whose
        // ids are numbers), TeXML apps, credential connections, messaging
        // profiles. A resource whose id is `gone` was deleted.
        let made = [
            ("/billing_groups", "bg"),
            ("/outbound_voice_profiles", "ovp"),
            ("/texml_applications", "app"),
            ("/credential_connections", "conn"),
            ("/messaging_profiles", "mp"),
        ];
        if method == "POST"
            && let Some((_, kind)) = made.iter().find(|(made_path, _)| *made_path == path)
        {
            let name = ["friendly_name", "connection_name", "name"]
                .iter()
                .find_map(|field| body[*field].as_str())
                .unwrap_or_default();
            if self
                .taken_names
                .lock()
                .unwrap()
                .iter()
                .any(|taken| taken == name)
            {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "errors": [{ "code": "10015", "title": "Bad Request",
                                              "detail": "The name you have chosen is already in use." }] })),
                )
                    .into_response();
            }
            let n = self.made.fetch_add(1, Ordering::SeqCst) + 1;
            let id = if *kind == "ovp" {
                json!(4_000_000_000_000_000_000_u64 + n as u64)
            } else {
                json!(format!("{kind}-new-{n}"))
            };
            return ok(json!({ "data": { "id": id, "name": name } }));
        }
        if method == "PATCH"
            && made
                .iter()
                .any(|(made_path, _)| path == format!("{made_path}/gone"))
        {
            return not_found();
        }
        match method {
            "GET" if path.starts_with("/mobile_push_credentials") => ok(json!({ "data": [
                { "id": "push-android", "alias": "granite-phone-android", "type": "android" },
                { "id": "push-other", "alias": "someone-elses-app", "type": "android" },
            ] })),
            "GET" if path.starts_with("/available_phone_numbers?") => ok(json!({ "data": [
                { "phone_number": "+13175550188", "record_type": "available_phone_number",
                  "region_information": [
                      { "region_type": "country_code", "region_name": "US" },
                      { "region_type": "rate_center", "region_name": "INDIANAPOLIS" },
                      { "region_type": "state", "region_name": "IN" },
                      { "region_type": "location", "region_name": "INDIANAPOLIS" } ],
                  "cost_information": { "currency": "USD", "upfront_cost": "1.00000", "monthly_cost": "1.00000" } },
                { "phone_number": "+13175550189", "record_type": "available_phone_number",
                  "region_information": [], "cost_information": {} },
            ], "meta": { "total_results": 2 } })),
            "POST" if path == "/number_orders" => {
                if body["phone_numbers"][0]["phone_number"] == REFUSED_NUMBER {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "errors": [{ "code": "85001", "title": "Number not available" }] })),
                    )
                        .into_response();
                }
                let n = self.made.fetch_add(1, Ordering::SeqCst) + 1;
                ok(json!({ "data": { "id": format!("order-{n}"), "status": "pending" } }))
            }
            _ => self.telnyx_calling_answer(method, path, body),
        }
    }

    fn telnyx_calling_answer(&self, method: &str, path: &str, body: &Value) -> Response {
        let ok = |body: Value| (StatusCode::OK, Json(body)).into_response();
        match method {
            "GET" if path.starts_with("/phone_numbers?") => {
                let numbers: Vec<Value> = self
                    .phone_numbers
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, e164)| {
                        json!({ "id": id, "record_type": "phone_number", "phone_number": e164 })
                    })
                    .collect();
                ok(json!({ "data": numbers, "meta": { "page_number": 1, "total_pages": 1 } }))
            }
            "POST" if path == "/telephony_credentials" => {
                let n = self.credentials.fetch_add(1, Ordering::SeqCst) + 1;
                ok(json!({ "data": {
                    "id": format!("cred-new-{n}"), "record_type": "credential",
                    "sip_username": format!("gencrednew{n}"), "sip_password": "secret" } }))
            }
            "POST" if path.starts_with("/telephony_credentials/") && path.ends_with("/token") => (
                StatusCode::OK,
                [(CONTENT_TYPE, "text/plain")],
                format!("{JWT}\n"),
            )
                .into_response(),
            "POST" if path.starts_with("/texml/calls/") => {
                if body["To"] == REFUSED_NUMBER {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({ "errors": [{ "code": "10015", "title": "Invalid number" }] })),
                    )
                        .into_response();
                }
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                ok(json!({ "data": { "call_sid": format!("v3:sid-{n}"), "status": "queued" } }))
            }
            "GET" if path.starts_with("/recordings/") => {
                let id = path.trim_start_matches("/recordings/");
                match self.recordings.lock().unwrap().get(id).cloned() {
                    Some((mp3, millis, call)) => {
                        let mut data = json!({
                            "id": id, "record_type": "recording", "status": "completed",
                            "duration_millis": millis, "download_urls": { "mp3": mp3 } });
                        for (key, value) in call.as_object().into_iter().flatten() {
                            data[key] = value.clone();
                        }
                        ok(json!({ "data": data }))
                    }
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "errors": [{ "code": "10005", "title": "Resource not found" }] })),
                    )
                        .into_response(),
                }
            }
            _ => ok(json!({ "data": {} })),
        }
    }

    async fn answer(&self, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let path = parts.uri.path().to_string();
        let full = parts
            .uri
            .path_and_query()
            .map_or_else(String::new, ToString::to_string);
        if let Some(telnyx) = full.strip_prefix("/telnyx") {
            assert_eq!(
                parts.headers["authorization"],
                format!("Bearer {TELNYX_KEY}")
            );
            let method = parts.method.to_string();
            self.telnyx
                .lock()
                .unwrap()
                .push((method.clone(), telnyx.to_string(), json.clone()));
            if self.telnyx_down.load(Ordering::SeqCst)
                || (method == "DELETE" && self.fail_deletes.load(Ordering::SeqCst))
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "errors": [{ "code": "10007", "title": "Unexpected error" }] })),
                )
                    .into_response();
            }
            return self.telnyx_answer(&method, telnyx, &json);
        }
        if let Some(rest) = path.strip_prefix("/aai") {
            assert_eq!(parts.headers["authorization"], "aai-key");
            return self.assemblyai_answer(parts.method.as_str(), rest, &bytes, &json);
        }
        if path == "/openai/chat" {
            let voicemail = json["messages"][0]["content"]
                .as_str()
                .is_some_and(|prompt| {
                    prompt.starts_with("Summarize this outbound sales voicemail")
                        || prompt.starts_with("Summarize this voicemail a caller left")
                });
            self.chats.lock().unwrap().push(json);
            if self.chat_down.load(Ordering::SeqCst) {
                return (StatusCode::INTERNAL_SERVER_ERROR, "down").into_response();
            }
            let reply = self
                .voicemail_reply
                .lock()
                .unwrap()
                .clone()
                .filter(|_| voicemail)
                .unwrap_or_else(|| self.chat_reply.lock().unwrap().clone());
            return Json(json!({
                "model": "gpt-6-luna",
                "choices": [{ "message": { "content": reply }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 10 } }))
            .into_response();
        }
        if path == "/oauth/token" {
            return Json(json!({ "access_token": "access-1", "expires_in": 3600 })).into_response();
        }
        if path.starts_with("/fcm/") {
            if self.slow_pushes.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            let message = json["message"].clone();
            if message["token"] == GONE_PUSH {
                return (
                    StatusCode::NOT_FOUND,
                    Json(
                        json!({ "error": { "code": 404, "status": "NOT_FOUND", "details": [
                        { "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                          "errorCode": "UNREGISTERED" }] } }),
                    ),
                )
                    .into_response();
            }
            self.pushes.lock().unwrap().push(message);
            return Json(json!({ "name": "projects/granite-phone-test/messages/1" }))
                .into_response();
        }
        if let Some(name) = path.strip_prefix("/files/") {
            return match self.files.lock().unwrap().get(name).cloned() {
                Some(bytes) => ([(CONTENT_TYPE, "audio/mpeg")], bytes).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        if path == "/hooks/email/send" {
            self.emails.lock().unwrap().push(json);
            return Json(json!({ "message_id": "<report@ses.test>" })).into_response();
        }
        Json(json!({})).into_response()
    }
}

/// The API with Telnyx, FCM, the webhooks Lambda, AssemblyAI, OpenAI and
/// storage pointed at stand-ins (calls back to `PUBLIC_API`, recordings in
/// a `phone-recordings` bucket, job key [`JOB_KEY`]).
pub async fn phone_server(pool: MySqlPool) -> (TestServer, Stubs) {
    phone_server_with(pool, |state| state).await
}

/// [`phone_server`] with more changes to the API's state.
pub async fn phone_server_with(
    pool: MySqlPool,
    configure: impl FnOnce(AppState) -> AppState,
) -> (TestServer, Stubs) {
    let (state, stubs) = phone_state(pool).await;
    (TestServer::new(app(configure(state))), stubs)
}

/// The API's state of [`phone_server`], for tests that also call library
/// code with it.
pub async fn phone_state(pool: MySqlPool) -> (AppState, Stubs) {
    let stubs = Stubs::default();
    let handler = stubs.clone();
    let router = Router::new().fallback(move |request: Request| {
        let handler = handler.clone();
        async move { handler.answer(request).await }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{address}");
    *stubs.base.lock().unwrap() = base.clone();
    let account = json!({
        "type": "service_account",
        "project_id": "granite-phone-test",
        "private_key": TEST_KEY,
        "client_email": "push@granite-phone-test.iam.gserviceaccount.com",
        "token_uri": format!("{base}/oauth/token"),
    })
    .to_string();
    let state = AppState::new(pool, SECRET, ALLOWED_ORIGIN)
        .with_telnyx(Telnyx::new(Some(TELNYX_KEY), &format!("{base}/telnyx")))
        .with_push(Push::new(Source::Json(account), &format!("{base}/fcm")))
        .with_webhooks(WebhooksClient::new(
            Some(&format!("{base}/hooks")),
            Some("hooks-key"),
        ))
        .with_storage(Storage::memory())
        .with_transcriber(Transcriber::new(Some("aai-key"), &format!("{base}/aai")))
        .with_ai(Ai::new(Some("ai-key"), &format!("{base}/openai/chat")))
        .with_job_key(JOB_KEY)
        .with_app_url(APP_URL)
        .with_phone_config(PhoneConfig {
            public_api_url: Arc::from(PUBLIC_API),
            recordings_bucket: Some(Arc::from("phone-recordings")),
            // No Telnyx key in most tests: Call Control events come unsigned.
            allow_unsigned_webhooks: true,
            ..PhoneConfig::default()
        });
    (state, stubs)
}

/// The path of a company's Telnyx callback (`route` may carry a query).
pub fn hook_path(company_id: i32, route: &str) -> String {
    format!("/v1/webhooks/telnyx/{}/{route}", webhook_token(company_id))
}

/// Posts a TeXML callback the way Telnyx does (form-encoded, with the
/// company's fixture `AccountSid` `acct-{company_id}` unless the form names
/// one).
pub async fn post_texml(
    server: &TestServer,
    company_id: i32,
    route: &str,
    form: &[(&str, &str)],
) -> axum_test::TestResponse {
    let account = format!("acct-{company_id}");
    let mut fields: Vec<(&str, &str)> = form.to_vec();
    if !fields.iter().any(|(name, _)| *name == "AccountSid") {
        fields.insert(0, ("AccountSid", account.as_str()));
    }
    server
        .post(&hook_path(company_id, route))
        .form(&fields)
        .await
}

/// The TeXML document of `verbs`, as our answers are written.
pub fn texml_doc(verbs: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response>{verbs}</Response>")
}

/// A test-only Ed25519 key pair standing in for Telnyx's signing key.
pub struct Signer {
    pair: ring::signature::Ed25519KeyPair,
    /// The public key, base64 (`TELNYX_PUBLIC_KEY`).
    pub public_key: String,
}

impl Signer {
    pub fn new() -> Self {
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public_key =
            base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref());
        Self { pair, public_key }
    }

    /// Telnyx's `telnyx-signature-ed25519` of `body` sent at `timestamp`.
    pub fn sign(&self, timestamp: &str, body: &[u8]) -> String {
        use base64::Engine as _;
        let mut message = format!("{timestamp}|").into_bytes();
        message.extend_from_slice(body);
        base64::engine::general_purpose::STANDARD.encode(self.pair.sign(&message).as_ref())
    }

    /// The signature headers for `body`, signed now.
    pub fn headers(&self, body: &[u8]) -> [(String, String); 2] {
        let now = chrono::Utc::now().timestamp().to_string();
        [
            (
                "telnyx-signature-ed25519".to_string(),
                self.sign(&now, body),
            ),
            ("telnyx-timestamp".to_string(), now),
        ]
    }
}

/// [`PhoneConfig`] of the phone tests, checking Telnyx's signatures with
/// `public_key`.
pub fn signed_config(public_key: &str) -> PhoneConfig {
    PhoneConfig {
        public_api_url: Arc::from(PUBLIC_API),
        recordings_bucket: Some(Arc::from("phone-recordings")),
        public_key: Some(Arc::from(public_key)),
        ..PhoneConfig::default()
    }
}
