//! Access tokens for the `RingCentral` REST API.
//!
//! A company that pressed Connect has OAuth tokens on its `company` row,
//! encrypted with AES-256-GCM under `AES_KEY` (the CRM's `cryptoHelpers`);
//! the access token is refreshed through Granite Manager's own `RingCentral`
//! app. Companies set up before Connect existed paste JWT credentials
//! instead, exchanged for a token here. Port of the CRM's
//! `ringcentralOauth.server.ts` and `getAccessToken`.
//!
//! A rep may connect their own login too (`ringcentral_user_logins`): it
//! texts and reads as their extension, which the company's login may not.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SERVER_URL: &str = "https://platform.ringcentral.com";
/// Refresh a little early so a request never races the expiry.
const ACCESS_REFRESH_SKEW_SECS: i64 = 120;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("RingCentral API credentials not found")]
    NoCredentials,
    #[error("RingCentral token error {status}: {body}")]
    Token { status: u16, body: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A usable token and the server it belongs to.
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: String,
    pub server_url: String,
}

/// The server: the company's override, then `RINGCENTRAL_SERVER`, then
/// production.
pub fn server_url(company_override: Option<&str>) -> String {
    let env = std::env::var("RINGCENTRAL_SERVER").ok();
    company_override
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .or_else(|| env.as_deref().map(str::trim).filter(|url| !url.is_empty()))
        .unwrap_or(DEFAULT_SERVER_URL)
        .trim_end_matches('/')
        .to_string()
}

/// The 256-bit key from `AES_KEY` (64 hex digits).
pub fn env_key() -> Option<[u8; 32]> {
    parse_hex_key(std::env::var("AES_KEY").ok()?.trim())
}

fn parse_hex_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

/// `base64(iv[12] || tag[16] || ciphertext)`, as the CRM's `encrypt` writes it.
pub fn encrypt(key: &[u8; 32], plain: &str) -> String {
    let cipher = Aes256Gcm::new(key.into());
    let iv: [u8; 12] = rand::random();
    // aes-gcm appends the tag to the ciphertext; the CRM puts it first.
    let sealed = cipher
        .encrypt(Nonce::from_slice(&iv), plain.as_bytes())
        .unwrap_or_default();
    let (body, tag) = sealed.split_at(sealed.len().saturating_sub(16));
    let mut packed = Vec::with_capacity(12 + sealed.len());
    packed.extend_from_slice(&iv);
    packed.extend_from_slice(tag);
    packed.extend_from_slice(body);
    STANDARD.encode(packed)
}

/// The CRM's `decrypt`; `None` for anything that does not open.
pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(blob).ok()?.trim();
    let bytes = STANDARD.decode(text).ok()?;
    if bytes.len() < 28 {
        return None;
    }
    let (iv, rest) = bytes.split_at(12);
    let (tag, body) = rest.split_at(16);
    let mut sealed = body.to_vec();
    sealed.extend_from_slice(tag);
    let cipher = Aes256Gcm::new(key.into());
    let plain = cipher
        .decrypt(Nonce::from_slice(iv), sealed.as_ref())
        .ok()?;
    let plain = String::from_utf8(plain).ok()?.trim().to_string();
    (!plain.is_empty()).then_some(plain)
}

#[derive(sqlx::FromRow)]
struct OauthRow {
    access_token: Option<Vec<u8>>,
    refresh_token: Option<Vec<u8>>,
    access_expires: Option<i64>,
    refresh_expires: Option<i64>,
    server_url: Option<String>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[derive(serde::Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
}

async fn post_token(
    client: &reqwest::Client,
    server_url: &str,
    basic: &str,
    form: &[(&str, &str)],
) -> Result<TokenReply, AuthError> {
    let response = client
        .post(format!("{server_url}/restapi/oauth/token"))
        .header("Authorization", format!("Basic {basic}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(serde_urlencoded::to_string(form).unwrap_or_default())
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::Token {
            status: status.as_u16(),
            body: body.chars().take(500).collect(),
        });
    }
    Ok(response.json().await?)
}

/// Tokens from a refresh grant, with the times they end.
struct Fresh {
    access: String,
    refresh: String,
    access_expires: i64,
    refresh_expires: i64,
}

/// Trades `refresh` for new tokens through Granite Manager's own app;
/// `None` when Connect is not configured here.
async fn refresh_tokens(
    client: &reqwest::Client,
    refresh: &str,
) -> Result<Option<Fresh>, AuthError> {
    let (Ok(app_id), Ok(app_secret)) = (
        std::env::var("RINGCENTRAL_OAUTH_CLIENT_ID"),
        std::env::var("RINGCENTRAL_OAUTH_CLIENT_SECRET"),
    ) else {
        return Ok(None);
    };
    let basic = STANDARD.encode(format!("{}:{}", app_id.trim(), app_secret.trim()));
    let reply = post_token(
        client,
        &server_url(None),
        &basic,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
    )
    .await?;
    let (Some(access), Some(refresh)) = (reply.access_token, reply.refresh_token) else {
        return Err(AuthError::Token {
            status: 502,
            body: "RingCentral token response is incomplete".into(),
        });
    };
    let now = now_secs();
    Ok(Some(Fresh {
        access,
        refresh,
        access_expires: now + reply.expires_in.filter(|secs| *secs > 0).unwrap_or(3600),
        refresh_expires: now
            + reply
                .refresh_token_expires_in
                .filter(|secs| *secs > 0)
                .unwrap_or(604_800),
    }))
}

/// The OAuth token of a company that pressed Connect, refreshed when due;
/// `None` when it has no (live) connection.
async fn oauth_token(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
    key: &[u8; 32],
) -> Result<Option<AccessToken>, AuthError> {
    let row: Option<OauthRow> = sqlx::query_as(
        "SELECT ringcentral_access_token AS access_token, ringcentral_refresh_token AS refresh_token,
                ringcentral_access_expires AS access_expires,
                ringcentral_refresh_expires AS refresh_expires, ringcentral_server_url AS server_url
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let access = row
        .access_token
        .as_deref()
        .and_then(|blob| decrypt(key, blob));
    let refresh = row
        .refresh_token
        .as_deref()
        .and_then(|blob| decrypt(key, blob));
    let (Some(access), Some(refresh), Some(access_expires), Some(refresh_expires)) =
        (access, refresh, row.access_expires, row.refresh_expires)
    else {
        return Ok(None);
    };
    let server = server_url(row.server_url.as_deref());
    let now = now_secs();
    if access_expires - ACCESS_REFRESH_SKEW_SECS > now {
        return Ok(Some(AccessToken {
            token: access,
            server_url: server,
        }));
    }
    if refresh_expires <= now {
        // The refresh token is dead: the company has to press Connect again.
        sqlx::query(
            "UPDATE company SET ringcentral_access_token = NULL, ringcentral_refresh_token = NULL,
                    ringcentral_access_expires = NULL, ringcentral_refresh_expires = NULL,
                    ringcentral_account_id = NULL, ringcentral_connected_at = NULL,
                    ringcentral_connected_by = NULL
              WHERE id = ?",
        )
        .bind(company_id)
        .execute(pool)
        .await?;
        return Ok(None);
    }
    let Some(fresh) = refresh_tokens(client, &refresh).await? else {
        tracing::warn!(
            company_id,
            "RingCentral connect is not configured; cannot refresh"
        );
        return Ok(None);
    };
    sqlx::query(
        "UPDATE company SET ringcentral_access_token = ?, ringcentral_refresh_token = ?,
                ringcentral_access_expires = ?, ringcentral_refresh_expires = ?
          WHERE id = ?",
    )
    .bind(encrypt(key, &fresh.access))
    .bind(encrypt(key, &fresh.refresh))
    .bind(fresh.access_expires)
    .bind(fresh.refresh_expires)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(Some(AccessToken {
        token: fresh.access,
        server_url: server,
    }))
}

#[derive(sqlx::FromRow)]
struct LoginRow {
    user_id: i32,
    access_token: Vec<u8>,
    refresh_token: Vec<u8>,
    access_expires: i64,
    refresh_expires: i64,
}

/// The own sign-in of the rep on `extension_id` (Connect my `RingCentral`),
/// refreshed when due or when `renew`; `None` without a live one. A login
/// `RingCentral` ended or refused is forgotten: the rep connects again.
async fn login_token(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
    extension_id: &str,
    key: &[u8; 32],
    renew: bool,
) -> Result<Option<AccessToken>, AuthError> {
    let row: Option<LoginRow> = sqlx::query_as(
        "SELECT user_id, access_token, refresh_token, access_expires, refresh_expires
           FROM ringcentral_user_logins WHERE company_id = ? AND extension_id = ?",
    )
    .bind(company_id)
    .bind(extension_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let (Some(access), Some(refresh)) = (
        decrypt(key, &row.access_token),
        decrypt(key, &row.refresh_token),
    ) else {
        return Ok(None);
    };
    let now = now_secs();
    if !renew && row.access_expires - ACCESS_REFRESH_SKEW_SECS > now {
        return Ok(Some(AccessToken {
            token: access,
            server_url: server_url(None),
        }));
    }
    if row.refresh_expires > now {
        match refresh_tokens(client, &refresh).await {
            Ok(Some(fresh)) => {
                sqlx::query(
                    "UPDATE ringcentral_user_logins
                        SET access_token = ?, refresh_token = ?, access_expires = ?, refresh_expires = ?
                      WHERE user_id = ?",
                )
                .bind(encrypt(key, &fresh.access))
                .bind(encrypt(key, &fresh.refresh))
                .bind(fresh.access_expires)
                .bind(fresh.refresh_expires)
                .bind(row.user_id)
                .execute(pool)
                .await?;
                return Ok(Some(AccessToken {
                    token: fresh.access,
                    server_url: server_url(None),
                }));
            }
            Ok(None) => return Ok(None),
            // RingCentral refused the refresh token (signed out, password changed).
            Err(AuthError::Token {
                status: 400 | 401, ..
            }) => {}
            Err(error) => return Err(error),
        }
    }
    tracing::warn!(
        company_id,
        user_id = row.user_id,
        "a rep's RingCentral sign-in ended; they must connect again"
    );
    sqlx::query("DELETE FROM ringcentral_user_logins WHERE user_id = ?")
        .bind(row.user_id)
        .execute(pool)
        .await?;
    Ok(None)
}

/// A token for the company, as the rep on `extension` when they connected
/// their own `RingCentral` (the company's login may not text or read as
/// another user); else the company's ([`access_token`]).
pub async fn access_token_as(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
    extension: Option<&str>,
) -> Result<AccessToken, AuthError> {
    if let Some(extension) = extension
        && let Some(key) = env_key()
    {
        match login_token(pool, client, company_id, extension, &key, false).await {
            Ok(Some(token)) => return Ok(token),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(company_id, extension, %error, "rep's RingCentral sign-in unusable");
            }
        }
    }
    access_token(pool, client, company_id).await
}

/// Renews the reps' own sign-ins that would lapse within two days:
/// `RingCentral` ends a refresh token a week after its last use. Returns
/// how many were renewed.
pub async fn renew_lapsing_logins(
    pool: &MySqlPool,
    client: &reqwest::Client,
) -> Result<usize, AuthError> {
    let Some(key) = env_key() else {
        return Ok(0);
    };
    let lapsing: Vec<(i32, String)> = sqlx::query_as(
        "SELECT company_id, extension_id FROM ringcentral_user_logins
          WHERE refresh_expires < UNIX_TIMESTAMP() + 2 * 86400",
    )
    .fetch_all(pool)
    .await?;
    let mut renewed = 0;
    for (company_id, extension) in lapsing {
        match login_token(pool, client, company_id, &extension, &key, true).await {
            Ok(Some(_)) => renewed += 1,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(company_id, extension, %error, "renewing a rep's RingCentral sign-in failed");
            }
        }
    }
    Ok(renewed)
}

#[derive(sqlx::FromRow)]
struct JwtRow {
    client_id: Option<String>,
    client_secret: Option<String>,
    jwt: Option<String>,
    server_url: Option<String>,
}

/// JWT tokens by company, until they expire (per Lambda instance).
static JWT_CACHE: Mutex<Option<HashMap<i32, (AccessToken, Instant)>>> = Mutex::new(None);

fn cached_jwt(company_id: i32) -> Option<AccessToken> {
    let guard = JWT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let cached = guard.as_ref()?.get(&company_id).cloned();
    drop(guard);
    let (token, until) = cached?;
    (until > Instant::now()).then_some(token)
}

/// Forgets a JWT token the API refused.
pub fn forget(company_id: i32) {
    let mut guard = JWT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(map) = guard.as_mut() {
        map.remove(&company_id);
    }
}

async fn jwt_token(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
) -> Result<AccessToken, AuthError> {
    if let Some(token) = cached_jwt(company_id) {
        return Ok(token);
    }
    let row: Option<JwtRow> = sqlx::query_as(
        "SELECT ringcentral_client_id AS client_id, ringcentral_client_secret AS client_secret,
                ringcentral_jwt AS jwt, ringcentral_server_url AS server_url
           FROM company WHERE id = ?",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    let filled = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
    let Some(row) = row else {
        return Err(AuthError::NoCredentials);
    };
    let (Some(id), Some(secret), Some(jwt)) = (
        filled(row.client_id),
        filled(row.client_secret),
        filled(row.jwt),
    ) else {
        return Err(AuthError::NoCredentials);
    };
    let server = server_url(row.server_url.as_deref());
    let basic = STANDARD.encode(format!("{id}:{secret}"));
    let reply = post_token(
        client,
        &server,
        &basic,
        &[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", &jwt),
        ],
    )
    .await?;
    let Some(access) = reply.access_token else {
        return Err(AuthError::Token {
            status: 500,
            body: "RingCentral token response missing access_token".into(),
        });
    };
    let token = AccessToken {
        token: access,
        server_url: server,
    };
    let lifetime = u64::try_from(reply.expires_in.unwrap_or(3600) - 60).unwrap_or(0);
    JWT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(
            company_id,
            (
                token.clone(),
                Instant::now() + Duration::from_secs(lifetime),
            ),
        );
    Ok(token)
}

/// A token for the company: its OAuth connection first, else its JWT
/// credentials.
pub async fn access_token(
    pool: &MySqlPool,
    client: &reqwest::Client,
    company_id: i32,
) -> Result<AccessToken, AuthError> {
    if let Some(key) = env_key()
        && let Some(token) = oauth_token(pool, client, company_id, &key).await?
    {
        return Ok(token);
    }
    jwt_token(pool, client, company_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn tokens_round_trip_in_the_crm_format() {
        let key = parse_hex_key(KEY_HEX).unwrap();
        let sealed = encrypt(&key, "secret-token");
        assert_eq!(
            decrypt(&key, sealed.as_bytes()).as_deref(),
            Some("secret-token")
        );
        assert_eq!(decrypt(&key, b"garbage"), None);
        assert!(parse_hex_key("abc").is_none());
    }

    #[test]
    fn opens_what_node_sealed() {
        // The CRM's `encrypt('hello')` with KEY_HEX and an iv of sevens.
        let key = parse_hex_key(KEY_HEX).unwrap();
        let node = "BwcHBwcHBwcHBwcHwoZjQXd5Iz7b7X5cJCk6v2cPyjAA";
        assert_eq!(decrypt(&key, node.as_bytes()).as_deref(), Some("hello"));
    }

    #[test_db::test]
    async fn a_live_oauth_connection_wins(pool: MySqlPool) {
        let key = parse_hex_key(KEY_HEX).unwrap();
        sqlx::query(
            "INSERT INTO company (id, name, ringcentral_access_token, ringcentral_refresh_token,
                                  ringcentral_access_expires, ringcentral_refresh_expires, ringcentral_server_url)
             VALUES (77, 'Rc Co', ?, ?, ?, ?, 'https://rc.test/')",
        )
        .bind(encrypt(&key, "live-access").into_bytes())
        .bind(encrypt(&key, "live-refresh").into_bytes())
        .bind(now_secs() + 3600)
        .bind(now_secs() + 86_400)
        .execute(&pool)
        .await
        .unwrap();
        let token = oauth_token(&pool, &crate::http::client(), 77, &key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token.token, "live-access");
        assert_eq!(token.server_url, "https://rc.test");
    }

    #[test_db::test]
    async fn a_dead_refresh_token_disconnects(pool: MySqlPool) {
        let key = parse_hex_key(KEY_HEX).unwrap();
        sqlx::query(
            "INSERT INTO company (id, name, ringcentral_access_token, ringcentral_refresh_token,
                                  ringcentral_access_expires, ringcentral_refresh_expires)
             VALUES (78, 'Old Co', ?, ?, 1, 2)",
        )
        .bind(encrypt(&key, "old").into_bytes())
        .bind(encrypt(&key, "old").into_bytes())
        .execute(&pool)
        .await
        .unwrap();
        let token = oauth_token(&pool, &crate::http::client(), 78, &key)
            .await
            .unwrap();
        assert!(token.is_none());
        let cleared: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT ringcentral_access_token FROM company WHERE id = 78")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(cleared.is_none());
    }

    async fn sign_in(pool: &MySqlPool, key: &[u8; 32], access_expires: i64, refresh_expires: i64) {
        sqlx::query(
            "INSERT INTO ringcentral_user_logins
                    (user_id, company_id, extension_id, access_token, refresh_token,
                     access_expires, refresh_expires)
             VALUES (5, 77, '555', ?, ?, ?, ?)",
        )
        .bind(encrypt(key, "rep-access").into_bytes())
        .bind(encrypt(key, "rep-refresh").into_bytes())
        .bind(access_expires)
        .bind(refresh_expires)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test_db::test]
    async fn a_reps_sign_in_serves_their_extension_only(pool: MySqlPool) {
        let key = parse_hex_key(KEY_HEX).unwrap();
        sign_in(&pool, &key, now_secs() + 3600, now_secs() + 86_400).await;
        let client = crate::http::client();
        let token = login_token(&pool, &client, 77, "555", &key, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(token.token, "rep-access");
        let other_extension = login_token(&pool, &client, 77, "556", &key, false).await;
        assert!(other_extension.unwrap().is_none());
        let other_company = login_token(&pool, &client, 78, "555", &key, false).await;
        assert!(other_company.unwrap().is_none());
    }

    #[test_db::test]
    async fn a_reps_lapsed_sign_in_is_forgotten(pool: MySqlPool) {
        let key = parse_hex_key(KEY_HEX).unwrap();
        sign_in(&pool, &key, 1, 2).await;
        let token = login_token(&pool, &crate::http::client(), 77, "555", &key, false)
            .await
            .unwrap();
        assert!(token.is_none());
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ringcentral_user_logins")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
