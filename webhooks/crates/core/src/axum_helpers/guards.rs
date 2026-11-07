use crate::axum_helpers::access_token::{LegacyAuth, LegacyCheck, TokenScope};
use crate::libs::types::BasicResponse;
use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, StatusCode, request::Parts};
use common::access_token::Scope;
use common::telegram::bot::{
    Bot, ChatId, InlineKeyboardMarkup, Message, MessageId, ParseMode, RequestError,
};
use lambda_http::tracing;
use uuid::{Uuid, uuid};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;

pub const CORRECT_ID: Uuid = uuid!("9ca4dfa8-0eec-46cc-967f-3385624be883");

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
    #[cfg(any(test, feature = "test-support"))]
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
        #[cfg(any(test, feature = "test-support"))]
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

/// Builds the bot only. Telegram's secret header is checked by the route's
/// `AccessToken<TelegramWebhookUser>`.
impl<S> FromRequestParts<S> for TelegramBot
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    fn from_request_parts(
        _parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Ok(Self {
            bot: bot_from_env(),
        }))
    }
}

// Every route but the lead forms takes `AccessToken<...>` with one of the
// scopes below. The lead forms keep `Transitional<MarketingUser>` and still
// let everyone in, so no lead is ever lost; PostHog shows who has not
// switched (`webhook_legacy_auth`).

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_uuid_from_bearer(header: &str) -> Option<Uuid> {
    header
        .strip_prefix("Bearer ")
        .and_then(|token| Uuid::parse_str(token.trim()).ok())
}

/// Lead forms (website, Facebook, Make, Zapier): a `leads` token for the
/// company, or anything else. Every request still gets in; the old bearer
/// UUID is only told apart in `PostHog`.
pub struct MarketingUser;

impl TokenScope for MarketingUser {
    const SCOPE: Scope = Scope::Leads;
}

impl LegacyAuth for MarketingUser {
    const NAME: &'static str = "marketing_uuid";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        if header(headers, "authorization").and_then(parse_uuid_from_bearer) == Some(CORRECT_ID) {
            LegacyCheck::Valid
        } else {
            LegacyCheck::Unverified
        }
    }
}

/// The `api` Lambda: an internal token (`WEBHOOKS_KEY`) with the
/// `internal` scope. `REMIX_KEY` no longer works.
pub struct RemixBackend;

impl TokenScope for RemixBackend {
    const SCOPE: Scope = Scope::Internal;
}

/// `CloudTalk`'s webhooks: a `telephony` token for the route's company (an
/// internal one on `/cloudtalk/sms`, which has none).
pub struct CloudTalkWebhookUser;

impl TokenScope for CloudTalkWebhookUser {
    const SCOPE: Scope = Scope::Telephony;
}

/// The `/ringcentral/sms|call` routes: a `telephony` token. The subscription
/// handshake (`Validation-Token`) is answered before any check
/// (`ringcentral::receive::handshake`).
pub struct RingCentralWebhookUser;

impl TokenScope for RingCentralWebhookUser {
    const SCOPE: Scope = Scope::Telephony;
}

/// `RingCentral`'s own webhook push (`/ringcentral/notify`). It cannot send a
/// bearer, so the subscription carries the token as its `verificationToken`,
/// echoed in `Verification-Token`.
pub struct RingCentralNotificationUser;

impl TokenScope for RingCentralNotificationUser {
    const SCOPE: Scope = Scope::Telephony;
}

/// Telegram's bot webhook: a `telegram` token as the webhook's
/// `secret_token` (`X-Telegram-Bot-Api-Secret-Token`). `WEBHOOK_SECRET` no
/// longer works.
pub struct TelegramWebhookUser;

impl TokenScope for TelegramWebhookUser {
    const SCOPE: Scope = Scope::Telegram;
}

/// SES events from `EventBridge`.
///
/// An access token (scope `ses`) in the API destination connection's
/// `Authorization`.
pub struct SesEventBridge;

impl TokenScope for SesEventBridge {
    const SCOPE: Scope = Scope::Ses;
}

/// Telnyx's `TeXML` call-progress callbacks.
///
/// An access token (scope `telephony`, internal) in the callback URL
/// (`?token=gmk_...`). Telnyx signs no `TeXML` callback and sets no
/// `Authorization`, so the URL is all it can carry.
pub struct TelnyxCallback;

impl TokenScope for TelnyxCallback {
    const SCOPE: Scope = Scope::Telephony;
    const TOKEN_IN_URL: bool = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn marketing_lets_everyone_in_but_tells_the_uuid_apart() {
        let mut with_uuid = HeaderMap::new();
        with_uuid.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {CORRECT_ID}")).unwrap(),
        );
        assert_eq!(MarketingUser::check(&with_uuid), LegacyCheck::Valid);
        assert_eq!(
            MarketingUser::check(&HeaderMap::new()),
            LegacyCheck::Unverified
        );
    }
}
