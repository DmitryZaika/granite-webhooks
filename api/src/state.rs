use crate::storage::Storage;
use crate::telegram::TelegramBots;
use crate::webhooks_client::WebhooksClient;
use axum::http::{Method, header};
use sqlx::MySqlPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Origins allowed when `API_ALLOWED_ORIGINS` is unset: the production tenant
/// subdomains. Local dev adds `http://localhost:5173` through the env file.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://granite-manager.com,https://*.granite-manager.com";

#[derive(Clone)]
pub struct AppState {
    pub pool: MySqlPool,
    /// Same value as the Remix app's `SESSION_SECRET`; signs the `__session` cookie.
    pub session_secret: Arc<str>,
    pub allowed_origins: Arc<[String]>,
    /// Bucket for uploaded images.
    pub storage: Storage,
    /// The two Telegram bots (lead alerts, notifications), to check a user's
    /// link still works.
    pub telegram: TelegramBots,
    /// The `webhooks` Lambda, for work it owns (CloudTalk contact sync).
    pub webhooks: WebhooksClient,
    /// When each SMS provider and company last had its upkeep run.
    pub sms_upkeep: Arc<Mutex<HashMap<(&'static str, i32), Instant>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("SESSION_SECRET environment variable must be set")]
pub struct MissingSessionSecret;

impl AppState {
    pub fn new(pool: MySqlPool, session_secret: &str, allowed_origins: &str) -> Self {
        Self {
            pool,
            session_secret: Arc::from(session_secret),
            allowed_origins: parse_origins(allowed_origins).into(),
            storage: Storage::Disabled,
            telegram: TelegramBots::disabled(),
            webhooks: WebhooksClient::disabled(),
            sms_upkeep: Arc::default(),
        }
    }

    #[must_use]
    pub fn with_storage(mut self, storage: Storage) -> Self {
        self.storage = storage;
        self
    }

    #[must_use]
    pub fn with_telegram(mut self, telegram: TelegramBots) -> Self {
        self.telegram = telegram;
        self
    }

    #[must_use]
    pub fn with_webhooks(mut self, webhooks: WebhooksClient) -> Self {
        self.webhooks = webhooks;
        self
    }

    pub fn from_env(pool: MySqlPool) -> Result<Self, MissingSessionSecret> {
        let secret = std::env::var("SESSION_SECRET").map_err(|_| MissingSessionSecret)?;
        let origins = std::env::var("API_ALLOWED_ORIGINS")
            .unwrap_or_else(|_| DEFAULT_ALLOWED_ORIGINS.to_string());
        Ok(Self::new(pool, &secret, &origins)
            .with_storage(Storage::from_env())
            .with_telegram(TelegramBots::from_env())
            .with_webhooks(WebhooksClient::from_env()))
    }

    /// Credentialed CORS: the browser sends the `__session` cookie, so the
    /// origin must be echoed back exactly (no `*`).
    pub fn cors_layer(&self) -> CorsLayer {
        let origins = Arc::clone(&self.allowed_origins);
        CorsLayer::new()
            .allow_origin(AllowOrigin::predicate(move |origin, _| {
                origin
                    .to_str()
                    .is_ok_and(|origin| origin_allowed(&origins, origin))
            }))
            .allow_credentials(true)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ])
            .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
    }
}

fn parse_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect()
}

/// Exact match, or `scheme://*.domain` matching any single-or-multi level subdomain.
pub fn origin_allowed(allowed: &[String], origin: &str) -> bool {
    allowed.iter().any(|pattern| {
        if let Some((scheme, domain)) = pattern.split_once("://*.") {
            origin
                .strip_prefix(scheme)
                .and_then(|rest| rest.strip_prefix("://"))
                .and_then(|host| host.strip_suffix(domain))
                .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.'))
        } else {
            pattern == origin
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{origin_allowed, parse_origins};

    #[test]
    fn wildcard_origins_match_subdomains_only() {
        let allowed = parse_origins("https://*.granite-manager.com, http://localhost:5173");
        assert!(origin_allowed(
            &allowed,
            "https://granitedepotindy.granite-manager.com"
        ));
        assert!(origin_allowed(&allowed, "http://localhost:5173"));
        assert!(!origin_allowed(&allowed, "https://granite-manager.com"));
        assert!(!origin_allowed(&allowed, "https://evilgranite-manager.com"));
        assert!(!origin_allowed(&allowed, "http://x.granite-manager.com"));
        assert!(!origin_allowed(&allowed, "http://localhost:3000"));
    }
}
