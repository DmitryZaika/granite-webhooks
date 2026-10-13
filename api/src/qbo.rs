//! `QuickBooks` Online: the company's connection, token refresh and API calls.
//!
//! Port of the CRM's `quickbooks.server.ts`. Tokens and app credentials are
//! stored encrypted with `AES_KEY` (the CRM's `cryptoHelpers`). A refresh
//! holds the company row lock until the new tokens are saved, so two
//! requests never spend the same refresh token.

use common::telephony::ringcentral_auth::{decrypt, encrypt, env_key};
use lambda_http::tracing;
use serde_json::Value;
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PRODUCTION_API: &str = "https://quickbooks.api.intuit.com";
const SANDBOX_API: &str = "https://sandbox-quickbooks.api.intuit.com";
const TOKEN_URL: &str = "https://oauth.platform.intuit.com/oauth2/v1/tokens/bearer";
const MINOR_VERSION: &str = "minorversion=75";
/// Refresh this many seconds before the access token expires.
const SAFETY_GAP: i64 = 60;
/// How long a report or query may take before it counts as failed.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Qbo {
    api_base: Arc<str>,
    token_url: Arc<str>,
    production: bool,
    aes_key: Option<[u8; 32]>,
    /// The shared app (`QBO_CLIENT_ID`/`QBO_CLIENT_SECRET`), for companies
    /// without their own.
    shared_app: Option<(Arc<str>, Arc<str>)>,
    http: reqwest::Client,
}

/// A usable access token.
#[derive(Debug, Clone)]
pub struct Access {
    pub token: String,
    pub realm_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum QboError {
    #[error("QuickBooks is not connected")]
    NotConnected,
    #[error("QuickBooks needs to be reconnected")]
    Reconnect,
    #[error("QuickBooks request failed: {0}")]
    Request(String),
    #[error("QuickBooks returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("database error")]
    Database(#[from] sqlx::Error),
}

impl QboError {
    /// Not connected, or the connection can no longer be refreshed.
    pub const fn is_not_connected(&self) -> bool {
        matches!(self, Self::NotConnected | Self::Reconnect)
    }
}

#[derive(sqlx::FromRow)]
struct ConnectionRow {
    quickbooks_enabled: bool,
    qbo_realm_id: Option<String>,
    qbo_access_token: Option<Vec<u8>>,
    qbo_refresh_token: Option<Vec<u8>>,
    qbo_access_expires: Option<i64>,
    qbo_refresh_expires: Option<i64>,
    qbo_client_id: Option<Vec<u8>>,
    qbo_client_secret: Option<Vec<u8>>,
}

struct Connection {
    realm_id: String,
    access_token: String,
    refresh_token: String,
    access_expires: i64,
    refresh_expires: i64,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A `QuickBooks` id: 1 to 32 digits.
pub fn valid_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// Quotes a value for a `QuickBooks` query.
pub fn escape(value: &str) -> String {
    value.replace('\'', "''")
}

impl Qbo {
    pub fn disabled() -> Self {
        Self::new(None, None, SANDBOX_API, TOKEN_URL, false)
    }

    /// `QBO_ENV=production` talks to the production API; anything else to
    /// the sandbox.
    pub fn from_env() -> Self {
        let production = std::env::var("QBO_ENV").is_ok_and(|env| env == "production");
        let id = std::env::var("QBO_CLIENT_ID").unwrap_or_default();
        let secret = std::env::var("QBO_CLIENT_SECRET").unwrap_or_default();
        Self::new(
            env_key(),
            Some((id.as_str(), secret.as_str())),
            if production {
                PRODUCTION_API
            } else {
                SANDBOX_API
            },
            TOKEN_URL,
            production,
        )
    }

    pub fn new(
        aes_key: Option<[u8; 32]>,
        shared_app: Option<(&str, &str)>,
        api_base: &str,
        token_url: &str,
        production: bool,
    ) -> Self {
        let shared_app = shared_app
            .map(|(id, secret)| (id.trim(), secret.trim()))
            .filter(|(id, secret)| id.len() >= 10 && secret.len() >= 10)
            .map(|(id, secret)| (Arc::from(id), Arc::from(secret)));
        Self {
            api_base: Arc::from(api_base.trim_end_matches('/')),
            token_url: Arc::from(token_url),
            production,
            aes_key,
            shared_app,
            http: reqwest::Client::new(),
        }
    }

    fn open(&self, blob: Option<&Vec<u8>>) -> Option<String> {
        decrypt(self.aes_key.as_ref()?, blob?)
    }

    async fn row(
        executor: impl sqlx::MySqlExecutor<'_>,
        company_id: i32,
        lock: bool,
    ) -> Result<Option<ConnectionRow>, sqlx::Error> {
        let sql = format!(
            "SELECT quickbooks_enabled <> 0 AS quickbooks_enabled, qbo_realm_id, qbo_access_token,
                    qbo_refresh_token, qbo_access_expires, qbo_refresh_expires, qbo_client_id,
                    qbo_client_secret
               FROM company WHERE id = ?{}",
            if lock { " FOR UPDATE" } else { "" }
        );
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(company_id)
            .fetch_optional(executor)
            .await
    }

    fn connection(&self, row: &ConnectionRow) -> Option<Connection> {
        let realm_id = row
            .qbo_realm_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())?;
        Some(Connection {
            realm_id: realm_id.to_string(),
            access_token: self.open(row.qbo_access_token.as_ref())?,
            refresh_token: self.open(row.qbo_refresh_token.as_ref())?,
            access_expires: row.qbo_access_expires?,
            refresh_expires: row.qbo_refresh_expires?,
        })
    }

    /// The company's own app credentials, else the shared app.
    fn app(&self, row: &ConnectionRow) -> Option<(String, String)> {
        let own = self
            .open(row.qbo_client_id.as_ref())
            .zip(self.open(row.qbo_client_secret.as_ref()))
            .filter(|(id, secret)| id.len() >= 10 && secret.len() >= 10);
        own.or_else(|| {
            self.shared_app
                .as_ref()
                .map(|(id, secret)| (id.to_string(), secret.to_string()))
        })
    }

    /// A valid access token for the company, refreshed when it is about to
    /// expire.
    pub async fn access(&self, pool: &MySqlPool, company_id: i32) -> Result<Access, QboError> {
        let row = Self::row(pool, company_id, false)
            .await?
            .ok_or(QboError::NotConnected)?;
        if !row.quickbooks_enabled {
            return Err(QboError::NotConnected);
        }
        let connection = self.connection(&row).ok_or(QboError::NotConnected)?;
        if now() < connection.access_expires - SAFETY_GAP {
            return Ok(Access {
                token: connection.access_token,
                realm_id: connection.realm_id,
            });
        }
        self.refresh(pool, company_id).await
    }

    async fn refresh(&self, pool: &MySqlPool, company_id: i32) -> Result<Access, QboError> {
        let mut tx = pool.begin().await?;
        let row = Self::row(&mut *tx, company_id, true)
            .await?
            .ok_or(QboError::NotConnected)?;
        let connection = self.connection(&row).ok_or(QboError::NotConnected)?;
        let now = now();
        // Another request refreshed while this one waited for the lock.
        if now < connection.access_expires - SAFETY_GAP {
            return Ok(Access {
                token: connection.access_token,
                realm_id: connection.realm_id,
            });
        }
        if now >= connection.refresh_expires - SAFETY_GAP {
            return Err(QboError::Reconnect);
        }
        if self.production {
            // Production tokens rotate: refreshing them from a copy of the
            // database would log the real one out.
            let database: Option<String> = sqlx::query_scalar("SELECT DATABASE()")
                .fetch_one(&mut *tx)
                .await?;
            if database.as_deref() != Some("main") {
                tracing::error!(
                    ?database,
                    "refusing a production QuickBooks refresh outside main"
                );
                return Err(QboError::Reconnect);
            }
        }
        let (client_id, client_secret) = self.app(&row).ok_or(QboError::NotConnected)?;
        let key = self.aes_key.as_ref().ok_or(QboError::NotConnected)?;
        let response = self
            .http
            .post(&*self.token_url)
            .basic_auth(client_id, Some(client_secret))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "grant_type=refresh_token&refresh_token={}",
                percent_encoding::utf8_percent_encode(
                    &connection.refresh_token,
                    percent_encoding::NON_ALPHANUMERIC
                )
            ))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|err| QboError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            tracing::warn!(company_id, status = %response.status(), "QuickBooks token refresh refused");
            return Err(QboError::Reconnect);
        }
        let tokens: Value = response
            .json()
            .await
            .map_err(|err| QboError::Request(err.without_url().to_string()))?;
        let (Some(access_token), Some(refresh_token), Some(access_in), Some(refresh_in)) = (
            tokens["access_token"].as_str(),
            tokens["refresh_token"].as_str(),
            tokens["expires_in"].as_i64().filter(|secs| *secs > 0),
            tokens["x_refresh_token_expires_in"]
                .as_i64()
                .filter(|secs| *secs > 0),
        ) else {
            return Err(QboError::Reconnect);
        };
        sqlx::query(
            "UPDATE company SET qbo_access_token = ?, qbo_refresh_token = ?, qbo_access_expires = ?,
                    qbo_refresh_expires = ? WHERE id = ?",
        )
        .bind(encrypt(key, access_token))
        .bind(encrypt(key, refresh_token))
        .bind(now + access_in)
        .bind(now + refresh_in)
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Access {
            token: access_token.to_string(),
            realm_id: connection.realm_id,
        })
    }

    fn url(&self, access: &Access, path: &str) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!(
            "{}/v3/company/{}/{path}{separator}{MINOR_VERSION}",
            self.api_base, access.realm_id
        )
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, QboError> {
        self.send_within(request, Duration::from_mins(1)).await
    }

    async fn send_within(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<reqwest::Response, QboError> {
        let response = request
            .timeout(timeout)
            .send()
            .await
            .map_err(|err| QboError::Request(err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(QboError::Status {
            status: status.as_u16(),
            body: body.chars().take(500).collect(),
        })
    }

    async fn json(&self, request: reqwest::RequestBuilder) -> Result<Value, QboError> {
        self.json_within(request, Duration::from_mins(1)).await
    }

    async fn json_within(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<Value, QboError> {
        self.send_within(request.header("Accept", "application/json"), timeout)
            .await?
            .json()
            .await
            .map_err(|err| QboError::Request(err.without_url().to_string()))
    }

    /// `GET /v3/company/{realm}/<path>`. Reports and queries give up after
    /// [`REPORT_TIMEOUT`] so a hung `QuickBooks` call can't keep a page
    /// (the dashboard's sales) spinning; other reads get a minute.
    pub async fn get(&self, access: &Access, path: &str) -> Result<Value, QboError> {
        let timeout = if path.starts_with("reports/") || path.starts_with("query?") {
            REPORT_TIMEOUT
        } else {
            Duration::from_mins(1)
        };
        self.json_within(
            self.http
                .get(self.url(access, path))
                .bearer_auth(&access.token),
            timeout,
        )
        .await
    }

    /// `POST /v3/company/{realm}/<path>` with a JSON body.
    pub async fn post(&self, access: &Access, path: &str, body: &Value) -> Result<Value, QboError> {
        self.json(
            self.http
                .post(self.url(access, path))
                .bearer_auth(&access.token)
                .json(body),
        )
        .await
    }

    /// The `entity` rows a query returns (`QueryResponse.<entity>`).
    pub async fn query(
        &self,
        access: &Access,
        query: &str,
        entity: &str,
    ) -> Result<Vec<Value>, QboError> {
        let encoded =
            percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC);
        let body = self.get(access, &format!("query?query={encoded}")).await?;
        Ok(match &body["QueryResponse"][entity] {
            Value::Array(rows) => rows.clone(),
            Value::Object(_) => vec![body["QueryResponse"][entity].clone()],
            _ => Vec::new(),
        })
    }

    /// An invoice as a PDF.
    pub async fn invoice_pdf(
        &self,
        access: &Access,
        invoice_id: &str,
    ) -> Result<Vec<u8>, QboError> {
        let response = self
            .send(
                self.http
                    .get(self.url(access, &format!("invoice/{invoice_id}/pdf")))
                    .bearer_auth(&access.token)
                    .header("Accept", "application/pdf"),
            )
            .await?;
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|err| QboError::Request(err.without_url().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, valid_id};

    #[test]
    fn ids_and_quotes() {
        assert!(valid_id("123"));
        assert!(!valid_id("12a"));
        assert!(!valid_id(""));
        assert_eq!(escape("O'Brien"), "O''Brien");
    }
}
