use crate::libs::types::BasicResponse;
use crate::posthog::{PostHogEvent, client};
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use common::telegram::bot::{
    Bot, ChatId, InlineKeyboardMarkup, Message, MessageId, ParseMode, RequestError,
};
use lambda_http::tracing;
use std::env::var;
use uuid::{Uuid, uuid};

use crate::axum_helpers::utils::get_remix_key;
use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};

pub(crate) const CORRECT_ID: Uuid = uuid!("9ca4dfa8-0eec-46cc-967f-3385624be883");

pub trait Telegram: Send + Sync {
    fn send_message<T>(
        &self,
        chat: ChatId,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send;

    fn send_repliable_message<T>(
        &self,
        chat: ChatId,
        text: T,
        repliable: InlineKeyboardMarkup,
        parse_mode: Option<ParseMode>,
    ) -> impl Future<Output = Result<Message, RequestError>> + Send
    where
        T: Into<String> + Send;

    fn edit_message_text<T>(
        &self,
        chat_id: i64,
        message_id: i32,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send;

    fn delete_message(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> impl Future<Output = Result<(), BasicResponse>> + Send;
}

#[derive(Clone)]
pub struct TelegramBot {
    bot: Bot,
}

impl Default for TelegramBot {
    fn default() -> Self {
        Self {
            bot: bot_from_env(),
        }
    }
}

/// `Bot::from_env()`, except that tests (which never load `.env`) get a
/// placeholder token instead of panicking.
fn bot_from_env() -> Bot {
    #[cfg(test)]
    if std::env::var("TELOXIDE_TOKEN").is_err() {
        return Bot::new("test-token");
    }
    Bot::from_env()
}

impl Telegram for TelegramBot {
    fn send_message<T>(
        &self,
        chat: ChatId,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            match bot.send_message(chat, &text, None, None).await {
                Ok(message) => Ok(message),
                Err(err) => {
                    tracing::error!(?err, ERR_SEND_TELEGRAM);
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }

    fn send_repliable_message<T>(
        &self,
        chat: ChatId,
        text: T,
        repliable: InlineKeyboardMarkup,
        parse_mode: Option<ParseMode>,
    ) -> impl Future<Output = Result<Message, RequestError>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            bot.send_message(chat, &text, Some(&repliable), parse_mode)
                .await
        }
    }

    fn edit_message_text<T>(
        &self,
        chat_id: i64,
        message_id: i32,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            match bot
                .edit_message_text(
                    ChatId(chat_id),
                    MessageId(message_id),
                    &text,
                    Some(&InlineKeyboardMarkup::default()),
                )
                .await
            {
                Ok(message) => Ok(message),
                Err(err) => {
                    tracing::error!(
                        ?err,
                        chat_id = chat_id,
                        message_id = message_id,
                        ERR_SEND_TELEGRAM
                    );
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }

    fn delete_message(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> impl Future<Output = Result<(), BasicResponse>> + Send {
        let bot = self.bot.clone();
        async move {
            match bot
                .delete_message(ChatId(chat_id), MessageId(message_id))
                .await
            {
                Ok(()) => Ok(()),
                Err(err) => {
                    tracing::error!(
                        ?err,
                        chat_id = chat_id,
                        message_id = message_id,
                        ERR_SEND_TELEGRAM
                    );
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct NotificationsTelegramBot {
    bot: Bot,
}

impl Default for NotificationsTelegramBot {
    fn default() -> Self {
        let token = std::env::var("TELOXIDE_NOTIFICATIONS_TOKEN")
            .or_else(|_| std::env::var("TELEGRAM_NOTIFICATIONS_BOT_TOKEN"));
        // Tests never load `.env`, so they never get (or use) the real token.
        #[cfg(test)]
        let token = token.or_else(|_| Ok::<_, std::env::VarError>("test-token".to_string()));
        let token = token.unwrap_or_else(|error| {
            panic!("TELOXIDE_NOTIFICATIONS_TOKEN must be set: {error}");
        });
        Self {
            bot: Bot::new(token),
        }
    }
}

impl Telegram for NotificationsTelegramBot {
    fn send_message<T>(
        &self,
        chat: ChatId,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            match bot.send_message(chat, &text, None, None).await {
                Ok(message) => Ok(message),
                Err(err) => {
                    tracing::error!(?err, ERR_SEND_TELEGRAM);
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }

    fn send_repliable_message<T>(
        &self,
        chat: ChatId,
        text: T,
        repliable: InlineKeyboardMarkup,
        parse_mode: Option<ParseMode>,
    ) -> impl Future<Output = Result<Message, RequestError>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            bot.send_message(chat, &text, Some(&repliable), parse_mode)
                .await
        }
    }

    fn edit_message_text<T>(
        &self,
        chat_id: i64,
        message_id: i32,
        text: T,
    ) -> impl Future<Output = Result<Message, BasicResponse>> + Send
    where
        T: Into<String> + Send,
    {
        let bot = self.bot.clone();
        let text = text.into();
        async move {
            match bot
                .edit_message_text(
                    ChatId(chat_id),
                    MessageId(message_id),
                    &text,
                    Some(&InlineKeyboardMarkup::default()),
                )
                .await
            {
                Ok(message) => Ok(message),
                Err(err) => {
                    tracing::error!(
                        ?err,
                        chat_id = chat_id,
                        message_id = message_id,
                        ERR_SEND_TELEGRAM
                    );
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }

    fn delete_message(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> impl Future<Output = Result<(), BasicResponse>> + Send {
        let bot = self.bot.clone();
        async move {
            match bot
                .delete_message(ChatId(chat_id), MessageId(message_id))
                .await
            {
                Ok(()) => Ok(()),
                Err(err) => {
                    tracing::error!(
                        ?err,
                        chat_id = chat_id,
                        message_id = message_id,
                        ERR_SEND_TELEGRAM
                    );
                    Err(internal_error(ERR_SEND_TELEGRAM))
                }
            }
        }
    }
}

impl<S> FromRequestParts<S> for TelegramBot
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let webhook_secret = var("WEBHOOK_SECRET").map_err(|e| {
            tracing::error!(?e, "failed to read WEBHOOK_SECRET environment variable");
            internal_error("failed to get webhook secret")
        })?;

        let secret = parts
            .headers
            .get("x-telegram-bot-api-secret-token")
            .and_then(|v| v.to_str().ok())
            .ok_or(FORBIDDEN_RESPONSE)?;

        if secret != webhook_secret {
            return Err(FORBIDDEN_RESPONSE);
        }
        Ok(Self {
            bot: bot_from_env(),
        })
    }
}

pub struct MarketingUser {}

impl MarketingUser {
    pub const fn new(_: uuid::Uuid) -> Self {
        Self {}
    }
}

fn parse_uuid_from_bearer(header: &str) -> Option<Uuid> {
    header
        .strip_prefix("Bearer ")
        .and_then(|token| Uuid::parse_str(token.trim()).ok())
}

async fn report_to_posthog(message: &str) {
    if message == "Authorization header not found" {
        return;
    }
    let Ok(api_key) = std::env::var("POSTHOG_API_KEY") else {
        tracing::error!("POSTHOG_API_KEY not set");
        return;
    };

    let event =
        PostHogEvent::new_general_exception(api_key, message, "Marketing Authorization Error");
    let posthog_client = client().await;
    let capture_result = posthog_client.capture(event).await;
    if let Err(err) = capture_result {
        tracing::error!("Error sending event to PostHog: {}", err);
    }
}

impl<S> FromRequestParts<S> for MarketingUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        let raw_bearer = if let Some(v) = headers.get("authorization") {
            v.to_str()
        } else {
            tracing::error!("Authorization header not found");
            report_to_posthog("Authorization header not found").await;
            return Ok(Self::new(CORRECT_ID));
        };
        let Ok(clean_bearer) = raw_bearer else {
            tracing::error!("Failed to parse authorization header");
            report_to_posthog("Failed to parse authorization header").await;
            return Ok(Self::new(CORRECT_ID));
        };

        let Some(bearer_uuid) = parse_uuid_from_bearer(clean_bearer) else {
            tracing::error!("Failed to parse bearer UUID: {}", clean_bearer);
            report_to_posthog("Failed to parse bearer UUID").await;
            return Ok(Self::new(CORRECT_ID));
        };

        if bearer_uuid != CORRECT_ID {
            tracing::error!("Bearer UUID does not match");
            report_to_posthog("Bearer UUID does not match").await;
            return Ok(Self::new(CORRECT_ID));
        }

        Ok(Self::new(bearer_uuid))
    }
}

pub struct RemixBackend {}

impl RemixBackend {
    pub fn new(_: String) -> Self {
        Self {}
    }
}

impl<S> FromRequestParts<S> for RemixBackend
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let headers = &parts.headers;
        let raw_bearer = if let Some(v) = headers.get("authorization") {
            v.to_str()
        } else {
            tracing::error!("Authorization header not found");
            report_to_posthog("Authorization header not found").await;
            return Err(FORBIDDEN_RESPONSE);
        };
        let Ok(clean_bearer) = raw_bearer else {
            tracing::error!("Failed to parse authorization header");
            report_to_posthog("Failed to parse authorization header").await;
            return Err(FORBIDDEN_RESPONSE);
        };

        if clean_bearer != get_remix_key() {
            tracing::error!("Bearer UUID does not match");
            report_to_posthog("Bearer UUID does not match").await;
            return Err(FORBIDDEN_RESPONSE);
        }

        Ok(Self::new(clean_bearer.to_string()))
    }
}

/// Strict auth for the `CloudTalk` SMS webhook: rejects any request whose bearer
/// token is missing/invalid/not equal to `CORRECT_ID` (unlike `MarketingUser`).
pub struct CloudTalkWebhookUser;

impl<S> FromRequestParts<S> for CloudTalkWebhookUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let bearer_uuid = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_uuid_from_bearer);
        match bearer_uuid {
            Some(uuid) if uuid == CORRECT_ID => Ok(Self),
            _ => {
                tracing::warn!("CloudTalk SMS webhook: missing or invalid bearer token");
                Err((StatusCode::FORBIDDEN, "Forbidden"))
            }
        }
    }
}

/// Same shared webhook bearer as CloudTalk (`CORRECT_ID`), for RingCentral routes.
pub struct RingCentralWebhookUser;

impl<S> FromRequestParts<S> for RingCentralWebhookUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // RingCentral subscription handshake: Validation-Token must be accepted
        // before auth so the webhook URL can be verified.
        if parts.headers.get("Validation-Token").is_some() {
            return Ok(Self);
        }
        let bearer_uuid = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_uuid_from_bearer);
        match bearer_uuid {
            Some(uuid) if uuid == CORRECT_ID => Ok(Self),
            _ => {
                tracing::warn!("RingCentral SMS webhook: missing or invalid bearer token");
                Err((StatusCode::FORBIDDEN, "Forbidden"))
            }
        }
    }
}

/// `RingCentral`'s own webhook push (`/ringcentral/notify`). It cannot send a bearer, so the
/// subscription carries `CORRECT_ID` as its `verificationToken`, echoed in `Verification-Token`.
pub struct RingCentralNotificationUser;

impl<S> FromRequestParts<S> for RingCentralNotificationUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Subscription handshake: accepted before auth so the address can be verified.
        if parts.headers.get("Validation-Token").is_some() {
            return Ok(Self);
        }
        let token = parts
            .headers
            .get("Verification-Token")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Uuid::parse_str(value.trim()).ok());
        match token {
            Some(uuid) if uuid == CORRECT_ID => Ok(Self),
            _ => {
                tracing::warn!("RingCentral notification: missing or invalid verification token");
                Err((StatusCode::FORBIDDEN, "Forbidden"))
            }
        }
    }
}
