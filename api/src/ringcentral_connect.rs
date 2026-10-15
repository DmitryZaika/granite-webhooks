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
    #[error(transparent)]
    Database(#[from] sqlx::Error),
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
        sqlx::query(
            "UPDATE company
                SET ringcentral_access_token = ?, ringcentral_refresh_token = ?,
                    ringcentral_access_expires = ?, ringcentral_refresh_expires = ?,
                    ringcentral_account_id = ?, ringcentral_connected_at = NOW(),
                    ringcentral_connected_by = ?
              WHERE id = ?",
        )
        .bind(encrypt(key, access))
        .bind(encrypt(key, refresh))
        .bind(now + expires_in)
        .bind(now + refresh_in)
        .bind(tokens["owner_id"].as_str())
        .bind(user_id)
        .bind(company_id)
        .execute(pool)
        .await?;
        Ok(())
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
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
