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
