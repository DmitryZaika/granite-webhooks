//! Access tokens: how callers of the webhooks Lambda prove who they are
//! (`migrations/20261025120000_create-access-tokens.sql`).
//!
//! A token is `gmk_` + 32 random bytes in base64url without padding (47
//! characters). Only its SHA-256 (hex) is stored. 256 random bits cannot be
//! guessed, so a fast hash is enough and lets the token be looked up by an
//! index; slow hashes (bcrypt, argon2) are for passwords people choose.
//!
//! A token works when it is not revoked, not expired, has the route's scope,
//! and, when it belongs to a company, the route's path names that company
//! (and the company is not deleted). [`TokenCache`] keeps what it read for
//! [`CACHE_TTL`], so the database is asked about a token at most once a
//! minute per Lambda instance, and a revocation takes at most that long.
//! Expiry is checked on every request, cached or not.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{NaiveDateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What every token starts with: it tells a token from the older shared
/// secrets, and secret scanners can spot one that leaks.
pub const TOKEN_PREFIX: &str = "gmk_";
/// Length of a token: the prefix and 32 bytes in base64url without padding.
pub const TOKEN_LEN: usize = TOKEN_PREFIX.len() + 43;
/// How long a looked-up token (or its absence) is trusted without asking
/// the database again.
pub const CACHE_TTL: Duration = Duration::from_secs(60);
/// `last_used_at` is written at most this often per token and instance.
const TOUCH_EVERY: Duration = Duration::from_secs(300);
/// The cache is emptied when it holds this many tokens: only well-formed
/// tokens reach it, so this takes someone sending made-up ones.
const CACHE_MAX_ENTRIES: usize = 10_000;

/// Which group of routes a token may call. Stored in the `scopes` SET column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Lead intake: website forms, Facebook, Make, Zapier.
    Leads,
    /// The CRM's own backend: sending email and texts, templates, sync.
    Internal,
    /// CloudTalk, RingCentral and Telnyx calling back.
    Telephony,
    /// Telegram's bot webhook.
    Telegram,
    /// SES events delivered by `EventBridge`.
    Ses,
}

impl Scope {
    pub const ALL: [Self; 5] = [
        Self::Leads,
        Self::Internal,
        Self::Telephony,
        Self::Telegram,
        Self::Ses,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leads => "leads",
            Self::Internal => "internal",
            Self::Telephony => "telephony",
            Self::Telegram => "telegram",
            Self::Ses => "ses",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == value)
    }

    /// The scopes in a SET column value (`"leads,telephony"`). Unknown names
    /// are skipped.
    pub fn parse_set(value: &str) -> Vec<Self> {
        value.split(',').filter_map(Self::parse).collect()
    }
}

/// A new token. Show it once; store only [`token_hash`].
pub fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// How a token is stored and looked up: its SHA-256, hex.
pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("write to String");
            hex
        })
}

/// The last 4 characters, stored to tell tokens apart.
pub fn token_hint(token: &str) -> &str {
    &token[token.len().saturating_sub(4)..]
}

/// True when `value` has the shape of a token (prefix, length, base64url).
/// Anything else is never looked up.
pub fn is_token_shaped(value: &str) -> bool {
    value.len() == TOKEN_LEN
        && value.starts_with(TOKEN_PREFIX)
        && value[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A stored token, as the checks need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessToken {
    pub id: i64,
    pub name: String,
    /// `None`: an internal token, good for every company.
    pub company_id: Option<i32>,
    pub scopes: Vec<Scope>,
    pub expires_at: Option<NaiveDateTime>,
    pub revoked_at: Option<NaiveDateTime>,
    /// The token's company is soft-deleted.
    pub company_deleted: bool,
}

/// Why a token was turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Denied {
    /// No token in the request.
    Missing,
    /// No such token.
    Unknown,
    Revoked,
    Expired,
    CompanyDeleted,
    /// The token does not have the route's scope.
    MissingScope,
    /// A company's token on a route for another company, or for none.
    WrongCompany,
}

impl Denied {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
            Self::CompanyDeleted => "company_deleted",
            Self::MissingScope => "missing_scope",
            Self::WrongCompany => "wrong_company",
        }
    }

    /// The token is fine but may not call this route (403, not 401).
    pub const fn is_forbidden(self) -> bool {
        matches!(self, Self::MissingScope | Self::WrongCompany)
    }
}

impl AccessToken {
    /// Whether this token may call a route of `scope` whose path names
    /// `company_id` (`None` when the path names no company) at `now` (UTC).
    pub fn authorize(
        &self,
        scope: Scope,
        company_id: Option<i32>,
        now: NaiveDateTime,
    ) -> Result<(), Denied> {
        if self.revoked_at.is_some() {
            return Err(Denied::Revoked);
        }
        if self.expires_at.is_some_and(|expires_at| expires_at <= now) {
            return Err(Denied::Expired);
        }
        if self.company_deleted {
            return Err(Denied::CompanyDeleted);
        }
        if !self.scopes.contains(&scope) {
            return Err(Denied::MissingScope);
        }
        match self.company_id {
            Some(own) if company_id != Some(own) => Err(Denied::WrongCompany),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("access token denied: {}", .0.as_str())]
    Denied(Denied),
    #[error("database error checking an access token: {0}")]
    Database(#[from] sqlx::Error),
}

struct CacheEntry {
    fetched_at: Instant,
    token: Option<AccessToken>,
}

/// Tokens read from the database, kept for a while (see the module docs).
/// One per process; tests make their own.
pub struct TokenCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, CacheEntry>>,
    touched: Mutex<HashMap<i64, Instant>>,
}

impl TokenCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::default(),
            touched: Mutex::default(),
        }
    }

    /// Checks `token` for a route of `scope` whose path names `company_id`,
    /// and notes that it was used.
    pub async fn verify(
        &self,
        pool: &MySqlPool,
        token: &str,
        scope: Scope,
        company_id: Option<i32>,
    ) -> Result<AccessToken, VerifyError> {
        if !is_token_shaped(token) {
            return Err(VerifyError::Denied(Denied::Unknown));
        }
        let found = self
            .lookup(pool, token)
            .await?
            .ok_or(VerifyError::Denied(Denied::Unknown))?;
        found
            .authorize(scope, company_id, Utc::now().naive_utc())
            .map_err(VerifyError::Denied)?;
        self.record_use(pool, found.id).await;
        Ok(found)
    }

    /// The stored token, from the cache while it is fresh.
    async fn lookup(
        &self,
        pool: &MySqlPool,
        token: &str,
    ) -> Result<Option<AccessToken>, sqlx::Error> {
        let hash = token_hash(token);
        if let Some(entry) = self.entries.lock().expect("token cache").get(&hash)
            && entry.fetched_at.elapsed() < self.ttl
        {
            return Ok(entry.token.clone());
        }
        let found = find_by_hash(pool, &hash).await?;
        let mut entries = self.entries.lock().expect("token cache");
        if entries.len() >= CACHE_MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            hash,
            CacheEntry {
                fetched_at: Instant::now(),
                token: found.clone(),
            },
        );
        Ok(found)
    }

    /// Writes `last_used_at`, at most every [`TOUCH_EVERY`]. A failed write
    /// is only logged: it must not turn a good request away.
    async fn record_use(&self, pool: &MySqlPool, id: i64) {
        {
            let mut touched = self.touched.lock().expect("token cache");
            if touched
                .get(&id)
                .is_some_and(|at| at.elapsed() < TOUCH_EVERY)
            {
                return;
            }
            touched.insert(id, Instant::now());
        }
        if let Err(error) = touch(pool, id).await {
            tracing::warn!(%error, token_id = id, "Failed to write access token last_used_at");
        }
    }
}

/// The token stored under `hash`, revoked or expired ones included.
pub async fn find_by_hash(
    pool: &MySqlPool,
    hash: &str,
) -> Result<Option<AccessToken>, sqlx::Error> {
    let row = sqlx::query!(
        r#"
        SELECT t.id, t.name, t.company_id, CAST(t.scopes AS CHAR) AS "scopes!: String",
               t.expires_at, t.revoked_at, c.deleted_at AS company_deleted_at
        FROM access_tokens t
        LEFT JOIN company c ON c.id = t.company_id
        WHERE t.token_hash = ?
        "#,
        hash
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| AccessToken {
        id: row.id,
        name: row.name,
        company_id: row.company_id,
        scopes: Scope::parse_set(&row.scopes),
        expires_at: row.expires_at,
        revoked_at: row.revoked_at,
        company_deleted: row.company_deleted_at.is_some(),
    }))
}

async fn touch(pool: &MySqlPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "UPDATE access_tokens SET last_used_at = UTC_TIMESTAMP() WHERE id = ?",
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// A token to store.
pub struct NewAccessToken<'a> {
    pub name: &'a str,
    pub company_id: Option<i32>,
    pub scopes: &'a [Scope],
    pub expires_at: Option<NaiveDateTime>,
    pub created_by: Option<&'a str>,
}

/// Makes a token, stores its hash, and returns its id and the token (shown
/// once, never stored).
pub async fn create(
    pool: &MySqlPool,
    new: &NewAccessToken<'_>,
) -> Result<(u64, String), sqlx::Error> {
    let token = new_token();
    let scopes = new
        .scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let result = sqlx::query!(
        r#"
        INSERT INTO access_tokens
            (name, company_id, scopes, token_hash, token_hint, created_at, created_by, expires_at)
        VALUES (?, ?, ?, ?, ?, UTC_TIMESTAMP(), ?, ?)
        "#,
        new.name,
        new.company_id,
        scopes,
        token_hash(&token),
        token_hint(&token),
        new.created_by,
        new.expires_at
    )
    .execute(pool)
    .await?;
    Ok((result.last_insert_id(), token))
}

/// A stored token as listed for people (never the token itself).
pub struct AccessTokenListing {
    pub id: i64,
    pub name: String,
    pub company_id: Option<i32>,
    pub scopes: String,
    pub token_hint: String,
    pub created_at: NaiveDateTime,
    pub created_by: Option<String>,
    pub expires_at: Option<NaiveDateTime>,
    pub revoked_at: Option<NaiveDateTime>,
    pub last_used_at: Option<NaiveDateTime>,
}

/// Every token, or `company_id`'s, newest first.
pub async fn list(
    pool: &MySqlPool,
    company_id: Option<i32>,
) -> Result<Vec<AccessTokenListing>, sqlx::Error> {
    sqlx::query_as!(
        AccessTokenListing,
        r#"
        SELECT id, name, company_id, CAST(scopes AS CHAR) AS "scopes!: String", token_hint,
               created_at, created_by, expires_at, revoked_at, last_used_at
        FROM access_tokens
        WHERE ? IS NULL OR company_id = ?
        ORDER BY id DESC
        "#,
        company_id,
        company_id
    )
    .fetch_all(pool)
    .await
}

/// Revokes token `id`. False when there is no such token or it was already
/// revoked.
pub async fn revoke(pool: &MySqlPool, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query!(
        "UPDATE access_tokens SET revoked_at = UTC_TIMESTAMP() WHERE id = ? AND revoked_at IS NULL",
        id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration as ChronoDuration, NaiveDate};

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 25)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn token(company_id: Option<i32>, scopes: &[Scope]) -> AccessToken {
        AccessToken {
            id: 1,
            name: "test".to_string(),
            company_id,
            scopes: scopes.to_vec(),
            expires_at: None,
            revoked_at: None,
            company_deleted: false,
        }
    }

    #[test]
    fn new_tokens_are_token_shaped_and_differ() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), TOKEN_LEN);
        assert!(is_token_shaped(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn only_token_shaped_values_are_looked_up() {
        assert!(!is_token_shaped("9ca4dfa8-0eec-46cc-967f-3385624be883"));
        assert!(!is_token_shaped("gmk_short"));
        assert!(!is_token_shaped(&format!("gmk_{}", "a".repeat(42) + "!")));
        assert!(!is_token_shaped(&format!("xyz_{}", "a".repeat(43))));
        assert!(is_token_shaped(&format!(
            "gmk_{}",
            "aZ0-_".repeat(8) + "abc"
        )));
    }

    #[test]
    fn hash_is_sha256_hex_and_hint_is_the_last_four() {
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(token_hint("gmk_abcdefgh"), "efgh");
    }

    #[test]
    fn scopes_round_trip_through_the_set_column() {
        assert_eq!(
            Scope::parse_set("leads,telephony,bogus"),
            vec![Scope::Leads, Scope::Telephony]
        );
        for scope in Scope::ALL {
            assert_eq!(Scope::parse(scope.as_str()), Some(scope));
        }
    }

    #[test]
    fn company_token_works_only_for_its_company() {
        let t = token(Some(7), &[Scope::Leads]);
        assert_eq!(t.authorize(Scope::Leads, Some(7), now()), Ok(()));
        assert_eq!(
            t.authorize(Scope::Leads, Some(8), now()),
            Err(Denied::WrongCompany)
        );
        assert_eq!(
            t.authorize(Scope::Leads, None, now()),
            Err(Denied::WrongCompany)
        );
    }

    #[test]
    fn internal_token_works_for_every_company_and_none() {
        let t = token(None, &[Scope::Internal]);
        assert_eq!(t.authorize(Scope::Internal, Some(7), now()), Ok(()));
        assert_eq!(t.authorize(Scope::Internal, None, now()), Ok(()));
    }

    #[test]
    fn scope_is_required() {
        let t = token(None, &[Scope::Leads]);
        assert_eq!(
            t.authorize(Scope::Internal, None, now()),
            Err(Denied::MissingScope)
        );
    }

    #[test]
    fn expired_revoked_and_deleted_company_are_denied() {
        let mut t = token(Some(7), &[Scope::Leads]);
        t.expires_at = Some(now() + ChronoDuration::seconds(1));
        assert_eq!(t.authorize(Scope::Leads, Some(7), now()), Ok(()));
        t.expires_at = Some(now());
        assert_eq!(
            t.authorize(Scope::Leads, Some(7), now()),
            Err(Denied::Expired)
        );

        let mut t = token(Some(7), &[Scope::Leads]);
        t.revoked_at = Some(now());
        assert_eq!(
            t.authorize(Scope::Leads, Some(7), now()),
            Err(Denied::Revoked)
        );

        let mut t = token(Some(7), &[Scope::Leads]);
        t.company_deleted = true;
        assert_eq!(
            t.authorize(Scope::Leads, Some(7), now()),
            Err(Denied::CompanyDeleted)
        );
    }

    async fn company(pool: &MySqlPool) -> i32 {
        sqlx::query("INSERT INTO company (name) VALUES ('Token Test Co')")
            .execute(pool)
            .await
            .unwrap()
            .last_insert_id()
            .try_into()
            .unwrap()
    }

    async fn create_token(
        pool: &MySqlPool,
        company_id: Option<i32>,
        scopes: &[Scope],
    ) -> (i64, String) {
        let (id, token) = create(
            pool,
            &NewAccessToken {
                name: "test",
                company_id,
                scopes,
                expires_at: None,
                created_by: Some("tests"),
            },
        )
        .await
        .unwrap();
        (id.try_into().unwrap(), token)
    }

    #[test_db::test]
    async fn created_token_verifies_and_records_use(pool: MySqlPool) {
        let company_id = company(&pool).await;
        let (id, token) = create_token(&pool, Some(company_id), &[Scope::Leads]).await;
        let cache = TokenCache::new(CACHE_TTL);

        let found = cache
            .verify(&pool, &token, Scope::Leads, Some(company_id))
            .await
            .unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.company_id, Some(company_id));

        let listed = list(&pool, Some(company_id)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].scopes, "leads");
        assert_eq!(listed[0].token_hint, token_hint(&token));
        assert!(listed[0].last_used_at.is_some());

        let stored: String = sqlx::query_scalar("SELECT token_hash FROM access_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, token_hash(&token));
        assert_ne!(stored, token);
    }

    #[test_db::test]
    async fn unknown_and_misused_tokens_are_denied(pool: MySqlPool) {
        let company_id = company(&pool).await;
        let (_, token) = create_token(&pool, Some(company_id), &[Scope::Leads]).await;
        let cache = TokenCache::new(CACHE_TTL);

        let denied = |result: Result<AccessToken, VerifyError>| match result {
            Err(VerifyError::Denied(denied)) => denied,
            other => panic!("expected a denial, got {other:?}"),
        };
        assert_eq!(
            denied(
                cache
                    .verify(&pool, &new_token(), Scope::Leads, Some(company_id))
                    .await
            ),
            Denied::Unknown
        );
        assert_eq!(
            denied(
                cache
                    .verify(&pool, "not-a-token", Scope::Leads, Some(company_id))
                    .await
            ),
            Denied::Unknown
        );
        assert_eq!(
            denied(
                cache
                    .verify(&pool, &token, Scope::Internal, Some(company_id))
                    .await
            ),
            Denied::MissingScope
        );
        assert_eq!(
            denied(
                cache
                    .verify(&pool, &token, Scope::Leads, Some(company_id + 1))
                    .await
            ),
            Denied::WrongCompany
        );
    }

    #[test_db::test]
    async fn revocation_waits_for_the_cache_to_expire(pool: MySqlPool) {
        let (id, token) = create_token(&pool, None, &[Scope::Internal]).await;
        let cached = TokenCache::new(CACHE_TTL);
        let uncached = TokenCache::new(Duration::ZERO);
        cached
            .verify(&pool, &token, Scope::Internal, None)
            .await
            .unwrap();

        assert!(revoke(&pool, id).await.unwrap());
        assert!(!revoke(&pool, id).await.unwrap());

        // Still in the cache for up to a minute...
        cached
            .verify(&pool, &token, Scope::Internal, None)
            .await
            .unwrap();
        // ... and refused once the database is asked again.
        assert!(matches!(
            uncached.verify(&pool, &token, Scope::Internal, None).await,
            Err(VerifyError::Denied(Denied::Revoked))
        ));
    }

    #[test_db::test]
    async fn deleted_company_token_is_denied(pool: MySqlPool) {
        let company_id = company(&pool).await;
        let (_, token) = create_token(&pool, Some(company_id), &[Scope::Leads]).await;
        sqlx::query("UPDATE company SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
            .bind(company_id)
            .execute(&pool)
            .await
            .unwrap();
        let cache = TokenCache::new(CACHE_TTL);
        assert!(matches!(
            cache
                .verify(&pool, &token, Scope::Leads, Some(company_id))
                .await,
            Err(VerifyError::Denied(Denied::CompanyDeleted))
        ));
    }
}
