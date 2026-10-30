//! Connecting a company's `RingCentral` account with OAuth.
//!
//! Granite Manager owns one `RingCentral` app
//! (`RINGCENTRAL_OAUTH_CLIENT_ID` / `RINGCENTRAL_OAUTH_CLIENT_SECRET`); a
//! company signs in with its ordinary login and the tokens are saved
//! encrypted (`AES_KEY`). Port of `ringcentralOauth.server.ts` and the SMS
//! subscription removal of `ringcentralSmsSubscription.server.ts`.

use common::telephony::ringcentral::{self, Body};
use common::telephony::ringcentral_auth::{encrypt, env_key, server_url};
use reqwest::Method;
use serde_json::Value;
use sqlx::MySqlPool;
use std::sync::Arc;
use std::time::Duration;

/// The app companies connect through.
#[derive(Clone)]
pub struct RingCentralApp {
    pub client_id: Arc<str>,
    pub client_secret: Arc<str>,
    /// `RingCentral`'s server (`RINGCENTRAL_SERVER`, else production).
    pub server: Arc<str>,
    /// Encrypts the saved tokens (`AES_KEY`).
    pub aes_key: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("RingCentral refused the code ({0})")]
    Token(u16),
    #[error("RingCentral request failed: {0}")]
    Request(String),
    #[error("RingCentral token response is incomplete")]
    Incomplete,
    #[error("AES_KEY is not set")]
    NoKey,
    /// A rep signed in to a `RingCentral` account other than the company's.
    #[error("the login belongs to another RingCentral account")]
    OtherAccount,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// The tokens a sign-in gave, with when they end.
struct Tokens {
    access: String,
    refresh: String,
    access_expires: i64,
    refresh_expires: i64,
    /// The extension that signed in.
    owner_id: Option<String>,
}

/// What a rep's own sign-in linked.
#[derive(Debug, Clone)]
pub struct RepLogin {
    /// Their `RingCentral` extension.
    pub extension_id: String,
    /// Their number that can text (last ten digits), if any.
    pub phone: Option<String>,
}

/// The number of an extension's `phone-number` list that texts: a direct
/// number with `SmsSender` first, then any number with it.
fn texting_number(numbers: &Value) -> Option<String> {
    let texting: Vec<&Value> = numbers["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| {
            record["features"]
                .as_array()
                .is_some_and(|features| features.iter().any(|f| f.as_str() == Some("SmsSender")))
        })
        .collect();
    texting
        .iter()
        .find(|record| record["usageType"].as_str() == Some("DirectNumber"))
        .or_else(|| texting.first())
        .and_then(|record| record["phoneNumber"].as_str())
        .map(crate::sms_provider::phone10)
        .filter(|digits| digits.len() == 10)
}

impl RingCentralApp {
    /// `None` when the app's credentials are not set (connect unavailable).
    pub fn from_env() -> Option<Self> {
        let id = std::env::var("RINGCENTRAL_OAUTH_CLIENT_ID").unwrap_or_default();
        let secret = std::env::var("RINGCENTRAL_OAUTH_CLIENT_SECRET").unwrap_or_default();
        Self::new(&id, &secret, &server_url(None), env_key())
    }

    pub fn new(id: &str, secret: &str, server: &str, aes_key: Option<[u8; 32]>) -> Option<Self> {
        let (id, secret) = (id.trim(), secret.trim());
        (!id.is_empty() && !secret.is_empty()).then(|| Self {
            client_id: Arc::from(id),
            client_secret: Arc::from(secret),
            server: Arc::from(server.trim_end_matches('/')),
            aes_key,
        })
    }

    /// `RingCentral`'s sign-in page.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> String {
        let encode = |value: &str| {
            percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
        };
        format!(
            "{}/restapi/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&state={}",
            self.server,
            encode(&self.client_id),
            encode(redirect_uri),
            encode(state),
        )
    }

    /// Trades the code for tokens and saves the company's connection.
    pub async fn connect(
        &self,
        pool: &MySqlPool,
        client: &reqwest::Client,
        company_id: i32,
        user_id: i32,
        code: &str,
        redirect_uri: &str,
    ) -> Result<(), ConnectError> {
        let key = self.aes_key.as_ref().ok_or(ConnectError::NoKey)?;
        let tokens = self.exchange(client, code, redirect_uri).await?;
        sqlx::query(
            "UPDATE company
                SET ringcentral_access_token = ?, ringcentral_refresh_token = ?,
                    ringcentral_access_expires = ?, ringcentral_refresh_expires = ?,
                    ringcentral_account_id = ?, ringcentral_connected_at = NOW(),
                    ringcentral_connected_by = ?
              WHERE id = ?",
        )
        .bind(encrypt(key, &tokens.access))
        .bind(encrypt(key, &tokens.refresh))
        .bind(tokens.access_expires)
        .bind(tokens.refresh_expires)
        .bind(tokens.owner_id)
        .bind(user_id)
        .bind(company_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Trades a rep's code for tokens and saves their own sign-in, once it
    /// is shown to be a user of the company's `RingCentral` account. An
    /// extension moves to the rep who signed in last.
    pub async fn connect_rep(
        &self,
        pool: &MySqlPool,
        client: &reqwest::Client,
        company_id: i32,
        user_id: i32,
        code: &str,
        redirect_uri: &str,
    ) -> Result<RepLogin, ConnectError> {
        let key = self.aes_key.as_ref().ok_or(ConnectError::NoKey)?;
        let tokens = self.exchange(client, code, redirect_uri).await?;
        let get = |path: &'static str| {
            let url = format!("{}/restapi/v1.0/{path}", self.server);
            let token = tokens.access.clone();
            async move {
                let response = client
                    .get(url)
                    .bearer_auth(token)
                    .header("Accept", "application/json")
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|err| ConnectError::Request(err.without_url().to_string()))?;
                if !response.status().is_success() {
                    return Err(ConnectError::Request(format!("{path}: {}", response.status())));
                }
                response
                    .json::<Value>()
                    .await
                    .map_err(|err| ConnectError::Request(err.without_url().to_string()))
            }
        };
        let me = get("account/~/extension/~").await?;
        let id_of = |value: &Value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_str().map(str::to_string))
        };
        let extension_id = id_of(&me["id"]).ok_or(ConnectError::Incomplete)?;
        let company_account = ringcentral::request(
            pool,
            client,
            company_id,
            Method::GET,
            "account/~",
            &Body::Empty,
            None,
        )
        .await
        .map_err(|error| ConnectError::Request(format!("company account: {}", error.status)))?;
        let account = id_of(&me["account"]["id"]);
        if account.is_none() || account != id_of(&company_account["id"]) {
            return Err(ConnectError::OtherAccount);
        }
        let phone = texting_number(&get("account/~/extension/~/phone-number?perPage=100").await?);
        let mut tx = pool.begin().await?;
        sqlx::query(
            "DELETE FROM ringcentral_user_logins
              WHERE (company_id = ? AND extension_id = ?) OR user_id = ?",
        )
        .bind(company_id)
        .bind(&extension_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ringcentral_user_logins
                    (user_id, company_id, extension_id, access_token, refresh_token,
                     access_expires, refresh_expires)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(&extension_id)
        .bind(encrypt(key, &tokens.access))
        .bind(encrypt(key, &tokens.refresh))
        .bind(tokens.access_expires)
        .bind(tokens.refresh_expires)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RepLogin {
            extension_id,
            phone,
        })
    }

    /// Trades an authorization code for tokens.
    async fn exchange(
        &self,
        client: &reqwest::Client,
        code: &str,
        redirect_uri: &str,
    ) -> Result<Tokens, ConnectError> {
        let form = serde_html_form::to_string([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .map_err(|err| ConnectError::Request(err.to_string()))?;
        let response = client
            .post(format!("{}/restapi/oauth/token", self.server))
            .basic_auth(&*self.client_id, Some(&*self.client_secret))
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|err| ConnectError::Request(err.without_url().to_string()))?;
        if !response.status().is_success() {
            return Err(ConnectError::Token(response.status().as_u16()));
        }
        let tokens: Value = response
            .json()
            .await
            .map_err(|err| ConnectError::Request(err.without_url().to_string()))?;
        let access = tokens["access_token"].as_str().unwrap_or_default();
        let refresh = tokens["refresh_token"].as_str().unwrap_or_default();
        if access.is_empty() || refresh.is_empty() {
            return Err(ConnectError::Incomplete);
        }
        let expires_in = tokens["expires_in"]
            .as_i64()
            .filter(|s| *s > 0)
            .unwrap_or(3600);
        let refresh_in = tokens["refresh_token_expires_in"]
            .as_i64()
            .filter(|s| *s > 0)
            .unwrap_or(604_800);
        let now = chrono::Utc::now().timestamp();
        Ok(Tokens {
            access: access.to_string(),
            refresh: refresh.to_string(),
            access_expires: now + expires_in,
            refresh_expires: now + refresh_in,
            owner_id: tokens["owner_id"].as_str().map(str::to_string),
        })
    }
}

/// Forgets the rep's own sign-in, after stopping `RingCentral` pushing texts
/// to it (while it still works). Whether there was one.
pub async fn disconnect_rep(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
    user_id: i32,
) -> Result<bool, sqlx::Error> {
    let extension: Option<String> = sqlx::query_scalar(
        "SELECT extension_id FROM ringcentral_user_logins WHERE user_id = ? AND company_id = ?",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(extension) = extension else {
        return Ok(false);
    };
    if let Err(error) =
        ringcentral::remove_rep_sms_subscription(pool, client, company_id, &extension).await
    {
        lambda_http::tracing::warn!(
            ?error,
            company_id,
            user_id,
            "could not remove a rep's RingCentral SMS subscription"
        );
    }
    sqlx::query("DELETE FROM ringcentral_user_logins WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// Whether texts from the CRM go out as the user: they connected their own
/// `RingCentral`, or theirs is the login the company connected with.
pub async fn texts_as_user(
    pool: &MySqlPool,
    company_id: i32,
    user_id: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ringcentral_user_logins WHERE user_id = ? AND company_id = ?)
             OR EXISTS(SELECT 1 FROM users u JOIN company c ON c.id = u.company_id
                        WHERE u.id = ? AND c.id = ?
                          AND COALESCE(TRIM(u.ringcentral_extension_id), '') <> ''
                          AND TRIM(u.ringcentral_extension_id) = c.ringcentral_account_id)",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(user_id)
    .bind(company_id)
    .fetch_one(pool)
    .await
}

/// The company pressed Connect and its refresh token is still alive.
pub async fn is_connected(pool: &MySqlPool, company_id: i32) -> Result<bool, sqlx::Error> {
    let connected: Option<bool> = sqlx::query_scalar(
        "SELECT ringcentral_access_token IS NOT NULL AND ringcentral_refresh_token IS NOT NULL
                AND ringcentral_access_expires IS NOT NULL
                AND ringcentral_refresh_expires > UNIX_TIMESTAMP()
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(connected.unwrap_or(false))
}

/// What a test call says about a saved connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, crate::openapi::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Connected,
    /// `RingCentral` ended the connection; the company must reconnect.
    Disconnected,
    /// `RingCentral` did not answer (rate limit, outage).
    Unverified,
}

/// Asks `RingCentral` whether the connection still works.
pub async fn check(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
) -> ConnectionStatus {
    match ringcentral::request(
        pool,
        client,
        company_id,
        Method::GET,
        "account/~/extension/~",
        &Body::Empty,
        None,
    )
    .await
    {
        Ok(_) => ConnectionStatus::Connected,
        Err(error) if matches!(error.status, 400 | 401) => ConnectionStatus::Disconnected,
        Err(_) => ConnectionStatus::Unverified,
    }
}

/// Stops `RingCentral` pushing the company's texts (while the token still
/// works), then forgets the connection.
pub async fn disconnect(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
) -> Result<(), sqlx::Error> {
    if let Some(address) = ringcentral::notify_address(company_id) {
        let removed = async {
            let subscriptions = ringcentral::request(
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
            if let Some(id) = existing {
                ringcentral::request(
                    pool,
                    client,
                    company_id,
                    Method::DELETE,
                    &format!("subscription/{id}"),
                    &Body::Empty,
                    None,
                )
                .await?;
            }
            Ok::<_, ringcentral::ApiError>(())
        }
        .await;
        if let Err(error) = removed {
            lambda_http::tracing::warn!(
                ?error,
                company_id,
                "could not remove the RingCentral SMS subscription"
            );
        }
    }
    sqlx::query(
        "UPDATE company
            SET ringcentral_access_token = NULL, ringcentral_refresh_token = NULL,
                ringcentral_access_expires = NULL, ringcentral_refresh_expires = NULL,
                ringcentral_account_id = NULL, ringcentral_connected_at = NULL,
                ringcentral_connected_by = NULL
          WHERE id = ?",
    )
    .bind(company_id)
    .execute(pool)
    .await?;
    common::telephony::ringcentral_auth::forget(company_id);
    Ok(())
}
