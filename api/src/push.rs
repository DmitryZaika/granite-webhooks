//! Our own pushes to the Granite Phone app: FCM HTTP v1 data messages that
//! carry ids only (missed call, voicemail, AI message, revoked, 911 alert).
//! Telnyx sends the incoming-call pushes itself. On Android they go at the
//! priority asked for, kept 60 s. An iPhone can't run code for a background
//! push it isn't sure to deliver, so it gets an alert with generic words
//! ("New text") that the app's notification service extension rewrites with
//! who it is from (`mutable-content`); `revoked` stays a background push
//! (`content-available`, priority 5, the only one Apple allows for them).
//!
//! Credentials are the Firebase service account: `FCM_SERVICE_ACCOUNT_JSON`
//! (the whole JSON), or `FCM_SERVICE_ACCOUNT_S3_KEY` (default `config/fcm-sa.json`), an object in
//! `PHONE_RECORDINGS_BUCKET` read on the first send (Lambda environment
//! variables max out at 4 KB). Its private key signs an RS256 JWT that the
//! token endpoint swaps for an OAuth access token, cached about 55 minutes.
//! `FCM_BASE_URL` points sends at a stub in tests (the token endpoint is the
//! account's `token_uri`). Without credentials every send is skipped with a
//! warning.

use crate::storage::Storage;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use lambda_http::tracing;
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// What an iPhone gets for `data`: an alert with the words Android's lock
/// screen shows (the app's notification service extension names the sender;
/// when it can't, these stay), or a background push for `revoked` and any
/// type this server doesn't word.
fn apns(data: &BTreeMap<String, String>) -> Value {
    let field = |key: &str| data.get(key).map(String::as_str);
    let (title, body) = match field("type") {
        Some("missed_call") => ("Missed call", None),
        Some("voicemail") => ("New voicemail", None),
        Some("ai_message") => ("Missed call. AI took a message.", None),
        Some("e911_alert") => (
            "911 was dialed",
            Some("Someone in your company dialed 911. Open Granite Phone for details.".to_string()),
        ),
        Some("text") => ("New text", None),
        Some("email") => ("New email", None),
        Some("notification") => ("CRM notification", None),
        Some("lead") => (
            match field("kind") {
                Some("repeat") => "Repeated lead",
                Some("assigned") => "Lead assigned to you",
                Some("quote") => "Online quote finished",
                _ => "New lead",
            },
            None,
        ),
        Some("maintenance") => (
            "Maintenance due today",
            match field("count") {
                Some("1") => Some("1 check is due.".to_string()),
                Some(count) if !count.is_empty() && count != "0" => {
                    Some(format!("{count} checks are due."))
                }
                _ => None,
            },
        ),
        _ => {
            return json!({
                "headers": { "apns-push-type": "background", "apns-priority": "5" },
                "payload": { "aps": { "content-available": 1 } },
            });
        }
    };
    let mut alert = json!({ "title": title });
    if let Some(body) = body {
        alert["body"] = Value::String(body);
    }
    json!({
        "headers": { "apns-push-type": "alert", "apns-priority": "10" },
        "payload": { "aps": { "alert": alert, "sound": "default", "mutable-content": 1 } },
    })
}

/// FCM's API root.
pub const FCM_BASE_URL: &str = "https://fcm.googleapis.com";
/// The service-account object in `PHONE_RECORDINGS_BUCKET` when
/// `FCM_SERVICE_ACCOUNT_S3_KEY` is unset (prod's Lambda env is at its 4 KB cap).
const DEFAULT_S3_KEY: &str = "config/fcm-sa.json";
/// Google's OAuth token endpoint, when the account names none.
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// What the access token may do: send FCM messages.
const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
/// How long an access token is reused (Google issues them for an hour).
const TOKEN_TTL: Duration = Duration::from_mins(55);
/// Every request gives up after this long.
const TIMEOUT: Duration = Duration::from_secs(10);
/// So a disabled sender warns once per container, not on every send.
static WARNED_DISABLED: AtomicBool = AtomicBool::new(false);

/// The FCM sender; cheap to clone (it shares its token cache).
#[derive(Clone)]
pub struct Push {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    source: Source,
    fcm_base: String,
    http: reqwest::Client,
    account: Mutex<Option<Arc<ServiceAccount>>>,
    token: Mutex<Option<(String, Instant)>>,
}

/// Where the Firebase service account comes from.
pub enum Source {
    /// The service-account JSON itself.
    Json(String),
    /// A JSON object in a bucket, read with the storage credentials on the
    /// first send.
    Bucket {
        storage: Storage,
        bucket: String,
        key: String,
    },
}

struct ServiceAccount {
    client_email: String,
    project_id: String,
    token_uri: String,
    key: RsaKeyPair,
}

/// What FCM did with a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Accepted for delivery.
    Sent,
    /// The app is gone from that phone (`UNREGISTERED`): clear its push token.
    Unregistered,
}

/// Why a push was not sent.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// No service account is configured: nothing was sent.
    #[error("push is not configured")]
    NotConfigured,
    /// The service account could not be read or used.
    #[error("FCM credentials are unusable: {0}")]
    Credentials(String),
    /// The request did not complete.
    #[error("FCM request failed: {0}")]
    Request(String),
    /// FCM answered with this error status.
    #[error("FCM answered {0}")]
    Status(u16),
}

impl Push {
    /// No credentials: every send is skipped ([`PushError::NotConfigured`]).
    pub const fn disabled() -> Self {
        Self { inner: None }
    }

    /// `FCM_SERVICE_ACCOUNT_JSON`, else `FCM_SERVICE_ACCOUNT_S3_KEY` (default
    /// `config/fcm-sa.json`) in `PHONE_RECORDINGS_BUCKET` (read through
    /// `storage`), else disabled;
    /// `FCM_BASE_URL` overrides the FCM root.
    pub fn from_env(storage: &Storage) -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let base = var("FCM_BASE_URL").unwrap_or_else(|| FCM_BASE_URL.to_string());
        if let Some(json) = var("FCM_SERVICE_ACCOUNT_JSON") {
            return Self::new(Source::Json(json), &base);
        }
        match var("PHONE_RECORDINGS_BUCKET") {
            Some(bucket) => Self::new(
                Source::Bucket {
                    storage: storage.clone(),
                    bucket,
                    key: var("FCM_SERVICE_ACCOUNT_S3_KEY")
                        .unwrap_or_else(|| DEFAULT_S3_KEY.to_string()),
                },
                &base,
            ),
            None => Self::disabled(),
        }
    }

    /// `fcm_base` is the FCM root, `https://fcm.googleapis.com` (a stub in tests).
    pub fn new(source: Source, fcm_base: &str) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                source,
                fcm_base: fcm_base.trim_end_matches('/').to_string(),
                http: reqwest::Client::builder()
                    .timeout(TIMEOUT)
                    .build()
                    .expect("HTTP client builds"),
                account: Mutex::new(None),
                token: Mutex::new(None),
            })),
        }
    }

    /// Credentials are configured.
    pub const fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Sends a data message (`data`: string values only, e.g.
    /// `{granite: "1", type: "missed_call", callId: "42"}`) to the phone with
    /// FCM registration `token`; `high_priority` wakes a sleeping phone.
    pub async fn send(
        &self,
        token: &str,
        data: &BTreeMap<String, String>,
        high_priority: bool,
    ) -> Result<Delivery, PushError> {
        let Some(inner) = &self.inner else {
            if !WARNED_DISABLED.swap(true, Ordering::Relaxed) {
                tracing::warn!("FCM is not configured; phone pushes are skipped");
            }
            return Err(PushError::NotConfigured);
        };
        let account = inner.account().await?;
        let message = json!({ "message": {
            "token": token,
            "data": data,
            "android": { "priority": if high_priority { "HIGH" } else { "NORMAL" }, "ttl": "60s" },
            "apns": apns(data),
        } });
        let url = format!(
            "{}/v1/projects/{}/messages:send",
            inner.fcm_base, account.project_id
        );
        // A rejected access token is fetched again once.
        for retry in [false, true] {
            let access = inner.access_token(&account).await?;
            let response = inner
                .http
                .post(&url)
                .bearer_auth(&access)
                .json(&message)
                .send()
                .await
                .map_err(|err| PushError::Request(err.without_url().to_string()))?;
            let status = response.status();
            if status.is_success() {
                return Ok(Delivery::Sent);
            }
            if status == StatusCode::UNAUTHORIZED && !retry {
                inner.forget_token();
                continue;
            }
            let answer: Value = response.json().await.unwrap_or(Value::Null);
            if status == StatusCode::NOT_FOUND || unregistered(&answer) {
                return Ok(Delivery::Unregistered);
            }
            return Err(PushError::Status(status.as_u16()));
        }
        Err(PushError::Status(StatusCode::UNAUTHORIZED.as_u16()))
    }
}

/// FCM's `UNREGISTERED` error code in an error answer.
fn unregistered(answer: &Value) -> bool {
    answer["error"]["details"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|detail| detail["errorCode"] == "UNREGISTERED")
}

impl Inner {
    fn cached_account(&self) -> Option<Arc<ServiceAccount>> {
        self.account
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The service account, read and parsed once.
    async fn account(&self) -> Result<Arc<ServiceAccount>, PushError> {
        if let Some(account) = self.cached_account() {
            return Ok(account);
        }
        let json = match &self.source {
            Source::Json(json) => json.clone(),
            Source::Bucket {
                storage,
                bucket,
                key,
            } => {
                let object = storage
                    .get_url(&format!("s3://{bucket}/{key}"))
                    .await
                    .map_err(|err| PushError::Credentials(err.to_string()))?
                    .ok_or_else(|| {
                        PushError::Credentials("the service account object is missing".into())
                    })?;
                String::from_utf8(object.bytes).map_err(|_| {
                    PushError::Credentials("the service account is not UTF-8".into())
                })?
            }
        };
        let account = Arc::new(ServiceAccount::parse(&json)?);
        *self.account.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&account));
        Ok(account)
    }

    fn cached_token(&self) -> Option<String> {
        self.token
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(token, _)| token.clone())
    }

    fn forget_token(&self) {
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// An OAuth access token for FCM, reused until shortly before it expires.
    async fn access_token(&self, account: &ServiceAccount) -> Result<String, PushError> {
        if let Some(token) = self.cached_token() {
            return Ok(token);
        }
        let assertion = account.jwt(chrono::Utc::now().timestamp())?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
            .append_pair("assertion", &assertion)
            .finish();
        let response = self
            .http
            .post(&account.token_uri)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|err| PushError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(PushError::Credentials(format!(
                "the token endpoint answered {}",
                response.status().as_u16()
            )));
        }
        let answer: Value = response
            .json()
            .await
            .map_err(|err| PushError::Request(err.without_url().to_string()))?;
        let token = answer["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| PushError::Credentials("no access token in the answer".into()))?
            .to_string();
        let lifetime = answer["expires_in"].as_u64().unwrap_or(3600);
        let ttl = TOKEN_TTL.min(Duration::from_secs(lifetime.saturating_sub(60)));
        *self.token.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((token.clone(), Instant::now() + ttl));
        Ok(token)
    }
}

impl ServiceAccount {
    /// A Google service-account JSON (`client_email`, `project_id`,
    /// `private_key` as a PKCS#8 PEM, optional `token_uri`).
    fn parse(json: &str) -> Result<Self, PushError> {
        let value: Value = serde_json::from_str(json)
            .map_err(|_| PushError::Credentials("the service account is not JSON".into()))?;
        let field = |name: &'static str| {
            value[name]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .ok_or_else(|| PushError::Credentials(format!("the service account has no {name}")))
        };
        let pem = field("private_key")?;
        let der: String = pem
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let der = STANDARD
            .decode(der)
            .map_err(|_| PushError::Credentials("the private key is not PEM".into()))?;
        let key = RsaKeyPair::from_pkcs8(&der)
            .map_err(|err| PushError::Credentials(format!("the private key is rejected: {err}")))?;
        Ok(Self {
            client_email: field("client_email")?,
            project_id: field("project_id")?,
            token_uri: field("token_uri").unwrap_or_else(|_| GOOGLE_TOKEN_URL.to_string()),
            key,
        })
    }

    /// The signed JWT the token endpoint swaps for an access token.
    fn jwt(&self, now: i64) -> Result<String, PushError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iss": self.client_email,
            "scope": SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + 3600,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| PushError::Credentials("signing the JWT failed".into()))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Request;
    use axum::response::IntoResponse;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use std::sync::atomic::AtomicUsize;

    /// A 2048-bit RSA key made for these tests only (`openssl genpkey`).
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
-----END PRIVATE KEY-----
";

    /// What the stand-in for Google saw.
    #[derive(Default)]
    struct Stub {
        token_requests: AtomicUsize,
        messages: Mutex<Vec<Value>>,
    }

    fn account_json(base: &str) -> String {
        json!({
            "type": "service_account",
            "project_id": "granite-phone-test",
            "private_key_id": "k1",
            "private_key": TEST_KEY,
            "client_email": "push@granite-phone-test.iam.gserviceaccount.com",
            "token_uri": format!("{base}/token"),
        })
        .to_string()
    }

    /// The public half of [`TEST_KEY`], to check the JWT's signature.
    fn public_key() -> Vec<u8> {
        let account = ServiceAccount::parse(&account_json("http://unused")).unwrap();
        account.key.public().as_ref().to_vec()
    }

    /// Google's token endpoint and FCM on 127.0.0.1.
    async fn google() -> (String, Arc<Stub>) {
        let stub = Arc::new(Stub::default());
        let seen = Arc::clone(&stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let audience = format!("{base}/token");
        let router = axum::Router::new().fallback(move |request: Request| {
            let seen = Arc::clone(&seen);
            let audience = audience.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                let bearer = parts
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                match parts.uri.path() {
                    "/token" => {
                        let form: BTreeMap<String, String> =
                            url::form_urlencoded::parse(&bytes).into_owned().collect();
                        assert_eq!(form["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
                        let jwt: Vec<&str> = form["assertion"].split('.').collect();
                        let signature = URL_SAFE_NO_PAD.decode(jwt[2]).unwrap();
                        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_key())
                            .verify(format!("{}.{}", jwt[0], jwt[1]).as_bytes(), &signature)
                            .expect("the JWT is signed with the account's key");
                        let claims: Value =
                            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(jwt[1]).unwrap()).unwrap();
                        assert_eq!(claims["iss"], "push@granite-phone-test.iam.gserviceaccount.com");
                        assert_eq!(claims["scope"], SCOPE);
                        assert_eq!(claims["aud"], audience);
                        assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 3600);
                        seen.token_requests.fetch_add(1, Ordering::SeqCst);
                        axum::Json(json!({ "access_token": "access-1", "expires_in": 3599, "token_type": "Bearer" }))
                            .into_response()
                    }
                    "/v1/projects/granite-phone-test/messages:send" => {
                        assert_eq!(bearer, "Bearer access-1");
                        let message: Value = serde_json::from_slice(&bytes).unwrap();
                        let token = message["message"]["token"].as_str().unwrap_or_default().to_string();
                        seen.messages.lock().unwrap().push(message);
                        match token.as_str() {
                            "gone" => (
                                StatusCode::NOT_FOUND,
                                axum::Json(json!({ "error": { "code": 404, "status": "NOT_FOUND", "details": [
                                    { "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                                      "errorCode": "UNREGISTERED" }] } })),
                            )
                                .into_response(),
                            "broken" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                            _ => axum::Json(json!({ "name": "projects/granite-phone-test/messages/1" }))
                                .into_response(),
                        }
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, stub)
    }

    fn data(kind: &str, call_id: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("granite".to_string(), "1".to_string()),
            ("type".to_string(), kind.to_string()),
            ("callId".to_string(), call_id.to_string()),
        ])
    }

    #[tokio::test]
    async fn pushes_are_sent_with_a_cached_access_token() {
        let (base, stub) = google().await;
        let push = Push::new(Source::Json(account_json(&base)), &format!("{base}/"));
        assert_eq!(
            push.send("good", &data("missed_call", "42"), true)
                .await
                .unwrap(),
            Delivery::Sent
        );
        assert_eq!(
            push.send("good", &data("voicemail", "43"), false)
                .await
                .unwrap(),
            Delivery::Sent
        );
        assert_eq!(stub.token_requests.load(Ordering::SeqCst), 1);
        let messages = stub.messages.lock().unwrap().clone();
        // An iPhone gets the same data with an alert its notification
        // service extension rewrites.
        assert_eq!(
            messages[0],
            json!({ "message": {
                "token": "good",
                "data": { "callId": "42", "granite": "1", "type": "missed_call" },
                "android": { "priority": "HIGH", "ttl": "60s" },
                "apns": {
                    "headers": { "apns-push-type": "alert", "apns-priority": "10" },
                    "payload": { "aps": {
                        "alert": { "title": "Missed call" },
                        "sound": "default",
                        "mutable-content": 1,
                    } },
                },
            } })
        );
        assert_eq!(
            messages[1]["message"]["android"],
            json!({ "priority": "NORMAL", "ttl": "60s" })
        );
        assert_eq!(
            messages[1]["message"]["apns"]["payload"]["aps"]["alert"],
            json!({ "title": "New voicemail" })
        );
    }

    #[test]
    fn an_iphone_gets_generic_words_or_a_background_push() {
        let alert = |data: &BTreeMap<String, String>| apns(data)["payload"]["aps"]["alert"].clone();
        let with = |kind: &str, extra: &[(&str, &str)]| {
            let mut data = data(kind, "1");
            data.extend(
                extra
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
            );
            data
        };
        assert_eq!(alert(&with("text", &[])), json!({ "title": "New text" }));
        assert_eq!(
            alert(&with("lead", &[("kind", "assigned")])),
            json!({ "title": "Lead assigned to you" })
        );
        assert_eq!(
            alert(&with("maintenance", &[("count", "3")])),
            json!({ "title": "Maintenance due today", "body": "3 checks are due." })
        );
        assert_eq!(
            alert(&with("e911_alert", &[]))["body"],
            "Someone in your company dialed 911. Open Granite Phone for details."
        );
        // Revoked (and a type this server doesn't word) stays a background push.
        let revoked = apns(&with("revoked", &[]));
        assert_eq!(revoked["headers"]["apns-push-type"], "background");
        assert_eq!(revoked["payload"]["aps"], json!({ "content-available": 1 }));
    }

    #[tokio::test]
    async fn gone_apps_and_errors_are_told_apart() {
        let (base, _) = google().await;
        let push = Push::new(Source::Json(account_json(&base)), &base);
        assert_eq!(
            push.send("gone", &data("missed_call", "1"), true)
                .await
                .unwrap(),
            Delivery::Unregistered
        );
        assert!(matches!(
            push.send("broken", &data("missed_call", "1"), true).await,
            Err(PushError::Status(500))
        ));
    }

    #[tokio::test]
    async fn the_account_can_live_in_the_bucket() {
        let (base, stub) = google().await;
        let storage = Storage::memory();
        storage
            .put(
                "phone/fcm.json",
                account_json(&base).into_bytes(),
                "application/json",
            )
            .await
            .unwrap();
        let push = Push::new(
            Source::Bucket {
                storage: storage.clone(),
                bucket: "granite-phone".into(),
                key: "phone/fcm.json".into(),
            },
            &base,
        );
        assert_eq!(
            push.send("good", &data("revoked", "0"), true)
                .await
                .unwrap(),
            Delivery::Sent
        );
        // Read once: a missing object later changes nothing.
        storage.delete("phone/fcm.json").await.unwrap();
        assert_eq!(
            push.send("good", &data("revoked", "0"), true)
                .await
                .unwrap(),
            Delivery::Sent
        );
        assert_eq!(stub.messages.lock().unwrap().len(), 2);
        let missing = Push::new(
            Source::Bucket {
                storage,
                bucket: "granite-phone".into(),
                key: "nope.json".into(),
            },
            &base,
        );
        assert!(matches!(
            missing.send("good", &data("revoked", "0"), true).await,
            Err(PushError::Credentials(_))
        ));
    }

    #[tokio::test]
    async fn without_credentials_nothing_is_sent() {
        let push = Push::disabled();
        assert!(!push.is_enabled());
        assert!(matches!(
            push.send("good", &data("missed_call", "1"), true).await,
            Err(PushError::NotConfigured)
        ));
        let bad = Push::new(
            Source::Json("{\"project_id\":\"x\"}".into()),
            "http://127.0.0.1:9",
        );
        assert!(matches!(
            bad.send("good", &data("missed_call", "1"), true).await,
            Err(PushError::Credentials(_))
        ));
    }
}
