//! The two Telegram bots a user links: lead alerts (`TELEGRAM_BOT_TOKEN`)
//! and CRM notifications (`TELEGRAM_NOTIFICATIONS_BOT_TOKEN`).
//!
//! A saved chat id is not proof the link works: the user may have blocked the
//! bot. `can_message` asks Telegram (`getChat`), like the Remix
//! `telegramBotDelivery.server.ts`. Remix also kept confirmed links in memory
//! for 10 minutes; a Lambda instance is short-lived, so every call asks.

use lambda_http::tracing;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

const TELEGRAM_API: &str = "https://api.telegram.org";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bot {
    Leads,
    Notifications,
}

#[derive(Clone)]
pub struct TelegramBots {
    leads_token: Option<Arc<str>>,
    notifications_token: Option<Arc<str>>,
    api_base: Arc<str>,
    http: reqwest::Client,
}

impl TelegramBots {
    /// No bots: nothing can be checked, and the bot ids are unknown.
    pub fn disabled() -> Self {
        Self::new(None, None, TELEGRAM_API)
    }

    pub fn from_env() -> Self {
        let token = |name| std::env::var(name).ok();
        Self::new(
            token("TELEGRAM_BOT_TOKEN").as_deref(),
            token("TELEGRAM_NOTIFICATIONS_BOT_TOKEN").as_deref(),
            TELEGRAM_API,
        )
    }

    /// `api_base` is `https://api.telegram.org`, or a stand-in in tests.
    pub fn new(
        leads_token: Option<&str>,
        notifications_token: Option<&str>,
        api_base: &str,
    ) -> Self {
        let token = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(Arc::from)
        };
        Self {
            leads_token: token(leads_token),
            notifications_token: token(notifications_token),
            api_base: Arc::from(api_base.trim_end_matches('/')),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .expect("HTTP client builds"),
        }
    }

    fn token(&self, bot: Bot) -> Option<&str> {
        match bot {
            Bot::Leads => self.leads_token.as_deref(),
            Bot::Notifications => self.notifications_token.as_deref(),
        }
    }

    /// The bot's user id, the part of its token before `:`; the browser's
    /// Telegram login widget needs it. None without a token.
    pub fn bot_id(&self, bot: Bot) -> Option<i64> {
        let id = self.token(bot)?.split(':').next()?.parse::<i64>().ok()?;
        (id > 0).then_some(id)
    }

    /// Sends `text` with one button opening `url`. Failures (no token,
    /// blocked bot, network) are logged and swallowed: notifications are best
    /// effort.
    pub async fn send_with_button(
        &self,
        bot: Bot,
        chat_id: i64,
        text: &str,
        label: &str,
        url: &str,
    ) {
        let Some(token) = self.token(bot) else {
            return;
        };
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": { "inline_keyboard": [[{ "text": label, "url": url }]] },
        });
        let result = self
            .http
            .post(format!("{}/bot{token}/sendMessage", self.api_base))
            .json(&body)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(?bot, status = %response.status(), "telegram sendMessage refused");
            }
            Err(err) => {
                tracing::warn!(?bot, error = %err.without_url(), "telegram sendMessage failed");
            }
        }
    }

    /// Sends `text` with an inline keyboard: rows of `{"text", "url"}` or
    /// `{"text", "callback_data"}` buttons. Returns the message id, or None
    /// when it was not sent (no token, blocked bot, network); failures are
    /// logged.
    pub async fn send_with_keyboard(
        &self,
        bot: Bot,
        chat_id: i64,
        text: &str,
        rows: &[Vec<serde_json::Value>],
    ) -> Option<i64> {
        #[derive(Deserialize)]
        struct Sent {
            message_id: i64,
        }
        #[derive(Deserialize)]
        struct Answer {
            ok: bool,
            result: Option<Sent>,
        }
        let token = self.token(bot)?;
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": { "inline_keyboard": rows },
        });
        let result = self
            .http
            .post(format!("{}/bot{token}/sendMessage", self.api_base))
            .json(&body)
            .send()
            .await;
        match result {
            Ok(response) if response.status().is_success() => response
                .json::<Answer>()
                .await
                .ok()
                .filter(|answer| answer.ok)
                .and_then(|answer| answer.result)
                .map(|sent| sent.message_id),
            Ok(response) => {
                tracing::warn!(?bot, status = %response.status(), "telegram sendMessage refused");
                None
            }
            Err(err) => {
                tracing::warn!(?bot, error = %err.without_url(), "telegram sendMessage failed");
                None
            }
        }
    }

    /// Whether `bot` can message `chat_id`: Some(true) yes, Some(false) no
    /// (blocked, chat gone), None when it could not be checked (no token,
    /// network error, unexpected answer).
    pub async fn can_message(&self, bot: Bot, chat_id: &str) -> Option<bool> {
        #[derive(Deserialize)]
        struct Answer {
            ok: bool,
        }
        let token = self.token(bot)?;
        let response = self
            .http
            .post(format!("{}/bot{token}/getChat", self.api_base))
            .json(&serde_json::json!({ "chat_id": chat_id }))
            .send()
            .await;
        match response {
            Ok(response) => response.json::<Answer>().await.ok().map(|answer| answer.ok),
            Err(err) => {
                tracing::warn!(?bot, error = %err.without_url(), "telegram getChat failed");
                None
            }
        }
    }
}

/// Seconds a Telegram login stays valid.
pub const LOGIN_MAX_AGE_SECONDS: i64 = 86_400;

/// The fields the Telegram login widget signs.
pub struct TelegramLogin<'a> {
    pub id: &'a str,
    pub auth_date: &'a str,
    pub first_name: &'a str,
    pub last_name: Option<&'a str>,
    pub username: Option<&'a str>,
    pub photo_url: Option<&'a str>,
}

impl TelegramLogin<'_> {
    /// `key=value` lines sorted by key, empty optional fields left out.
    fn data_check_string(&self) -> String {
        let mut entries = vec![
            ("auth_date", self.auth_date),
            ("first_name", self.first_name),
            ("id", self.id),
        ];
        for (key, value) in [
            ("last_name", self.last_name),
            ("photo_url", self.photo_url),
            ("username", self.username),
        ] {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                entries.push((key, value));
            }
        }
        entries.sort_by_key(|(key, _)| *key);
        entries
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `hash` is Telegram's signature of `login` with `token`.
pub fn login_signed(login: &TelegramLogin<'_>, hash: &str, token: &str) -> bool {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};
    let secret = Sha256::digest(token.as_bytes());
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&secret) else {
        return false;
    };
    mac.update(login.data_check_string().as_bytes());
    let Some(expected) = (0..hash.len())
        .step_by(2)
        .map(|i| {
            hash.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    mac.verify_slice(&expected).is_ok()
}

/// A login made in the last day (and not in the future).
pub const fn login_fresh(auth_date: i64, now: i64) -> bool {
    auth_date > 0 && auth_date <= now + 60 && now - auth_date <= LOGIN_MAX_AGE_SECONDS
}

impl TelegramBots {
    /// The bot's token, to check a login against.
    pub fn login_token(&self, bot: Bot) -> Option<&str> {
        self.token(bot)
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;

    #[test]
    fn a_login_is_checked_against_the_bot_token() {
        let login = TelegramLogin {
            id: "42",
            auth_date: "1700000000",
            first_name: "Rita",
            last_name: None,
            username: Some("rita"),
            photo_url: Some(""),
        };
        assert_eq!(
            login.data_check_string(),
            "auth_date=1700000000\nfirst_name=Rita\nid=42\nusername=rita"
        );
        // HMAC-SHA256(key = SHA256("123:abc"), data), computed with Node's crypto.
        let hash = "f2df0cd9464348b6d8b2d8fb0c786e78655d34066af315f76ee7c0bd659f007a";
        assert!(login_signed(&login, hash, "123:abc"));
        assert!(!login_signed(&login, hash, "123:other"));
        assert!(!login_signed(&login, "zz", "123:abc"));
        assert!(login_fresh(1_700_000_000, 1_700_000_100));
        assert!(!login_fresh(1_700_000_000, 1_700_100_000));
    }
}
