use crate::axum_helpers::access_token::{LegacyAuth, LegacyCheck, TokenScope};
use crate::libs::types::BasicResponse;
use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, StatusCode, request::Parts};
use common::access_token::Scope;
use common::telegram::bot::{
    Bot, ChatId, InlineKeyboardMarkup, Message, MessageId, ParseMode, RequestError,
};
use lambda_http::tracing;
use std::env::var;
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
/// `Transitional<TelegramWebhookUser>`.
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

// The checks below are from before access tokens. Each route keeps its own
// through `Transitional<...>` until its callers send tokens; then the route
// moves to `AccessToken<...>` and the check is deleted.

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_uuid_from_bearer(header: &str) -> Option<Uuid> {
    header
        .strip_prefix("Bearer ")
        .and_then(|token| Uuid::parse_str(token.trim()).ok())
}

fn bearer_is_correct_id(headers: &HeaderMap) -> bool {
    header(headers, "authorization").and_then(parse_uuid_from_bearer) == Some(CORRECT_ID)
}

const fn valid_or_rejected(valid: bool) -> LegacyCheck {
    if valid {
        LegacyCheck::Valid
    } else {
        LegacyCheck::Rejected
    }
}

/// Lead forms (website, Facebook, Make, Zapier): the shared bearer UUID was
/// asked for but never required, so every request still gets in.
pub struct MarketingUser;

impl TokenScope for MarketingUser {
    const SCOPE: Scope = Scope::Leads;
}

impl LegacyAuth for MarketingUser {
    const NAME: &'static str = "marketing_uuid";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        if bearer_is_correct_id(headers) {
            LegacyCheck::Valid
        } else {
            LegacyCheck::Unverified
        }
    }
}

/// The CRM's backend: `Authorization` equal to `REMIX_KEY`.
pub struct RemixBackend;

impl TokenScope for RemixBackend {
    const SCOPE: Scope = Scope::Internal;
}

impl LegacyAuth for RemixBackend {
    const NAME: &'static str = "remix_key";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        let Ok(remix_key) = var("REMIX_KEY") else {
            tracing::error!("REMIX_KEY is not set");
            return LegacyCheck::Rejected;
        };
        valid_or_rejected(header(headers, "authorization").is_some_and(|sent| sent == remix_key))
    }
}

/// `CloudTalk`'s webhooks: bearer `CORRECT_ID`, required.
pub struct CloudTalkWebhookUser;

impl TokenScope for CloudTalkWebhookUser {
    const SCOPE: Scope = Scope::Telephony;
}

impl LegacyAuth for CloudTalkWebhookUser {
    const NAME: &'static str = "cloudtalk_uuid";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        valid_or_rejected(bearer_is_correct_id(headers))
    }
}

/// The `/ringcentral/sms|call` routes: bearer `CORRECT_ID`, required. The
/// subscription handshake (`Validation-Token`) is answered before any check
/// (`ringcentral::receive::handshake`).
pub struct RingCentralWebhookUser;

impl TokenScope for RingCentralWebhookUser {
    const SCOPE: Scope = Scope::Telephony;
}

impl LegacyAuth for RingCentralWebhookUser {
    const NAME: &'static str = "ringcentral_uuid";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        valid_or_rejected(bearer_is_correct_id(headers))
    }
}

/// `RingCentral`'s own webhook push (`/ringcentral/notify`). It cannot send a
/// bearer, so the subscription carries `CORRECT_ID` as its
/// `verificationToken`, echoed in `Verification-Token`.
pub struct RingCentralNotificationUser;

impl TokenScope for RingCentralNotificationUser {
    const SCOPE: Scope = Scope::Telephony;
}

impl LegacyAuth for RingCentralNotificationUser {
    const NAME: &'static str = "ringcentral_verification_uuid";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        let token = header(headers, "verification-token")
            .and_then(|value| Uuid::parse_str(value.trim()).ok());
        valid_or_rejected(token == Some(CORRECT_ID))
    }
}

/// Telegram's bot webhook: `X-Telegram-Bot-Api-Secret-Token` equal to
/// `WEBHOOK_SECRET`.
pub struct TelegramWebhookUser;

impl TokenScope for TelegramWebhookUser {
    const SCOPE: Scope = Scope::Telegram;
}

impl LegacyAuth for TelegramWebhookUser {
    const NAME: &'static str = "telegram_webhook_secret";

    fn check(headers: &HeaderMap) -> LegacyCheck {
        let Ok(webhook_secret) = var("WEBHOOK_SECRET") else {
            tracing::error!("WEBHOOK_SECRET is not set");
            return LegacyCheck::Rejected;
        };
        valid_or_rejected(
            header(headers, "x-telegram-bot-api-secret-token")
                .is_some_and(|secret| secret == webhook_secret),
        )
    }
}

/// SES events from `EventBridge`. Its connection sends an `Authorization`
/// header that was never checked, so every request still gets in.
pub struct SesEventBridge;

impl TokenScope for SesEventBridge {
    const SCOPE: Scope = Scope::Ses;
}

impl LegacyAuth for SesEventBridge {
    const NAME: &'static str = "ses_unchecked";

    fn check(_: &HeaderMap) -> LegacyCheck {
        LegacyCheck::Unverified
    }
}

/// Telnyx's call-progress callbacks were never checked, so every request
/// still gets in.
pub struct TelnyxCallback;

impl TokenScope for TelnyxCallback {
    const SCOPE: Scope = Scope::Telephony;
}

impl LegacyAuth for TelnyxCallback {
    const NAME: &'static str = "telnyx_unchecked";

    fn check(_: &HeaderMap) -> LegacyCheck {
        LegacyCheck::Unverified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn marketing_lets_everyone_in_but_tells_the_uuid_apart() {
        let bearer = format!("Bearer {CORRECT_ID}");
        assert_eq!(
            MarketingUser::check(&headers(&[("authorization", &bearer)])),
            LegacyCheck::Valid
        );
        assert_eq!(
            MarketingUser::check(&HeaderMap::new()),
            LegacyCheck::Unverified
        );
        assert_eq!(
            MarketingUser::check(&headers(&[("authorization", "Bearer nope")])),
            LegacyCheck::Unverified
        );
    }

    #[test]
    fn telephony_checks_still_require_the_uuid() {
        let bearer = format!("Bearer {CORRECT_ID}");
        let with_bearer = headers(&[("authorization", &bearer)]);
        assert_eq!(
            CloudTalkWebhookUser::check(&with_bearer),
            LegacyCheck::Valid
        );
        assert_eq!(
            RingCentralWebhookUser::check(&with_bearer),
            LegacyCheck::Valid
        );
        assert_eq!(
            CloudTalkWebhookUser::check(&HeaderMap::new()),
            LegacyCheck::Rejected
        );
        assert_eq!(
            RingCentralWebhookUser::check(&HeaderMap::new()),
            LegacyCheck::Rejected
        );
        // A Validation-Token is no longer a way past the check.
        assert_eq!(
            RingCentralWebhookUser::check(&headers(&[("validation-token", "abc")])),
            LegacyCheck::Rejected
        );

        let id = CORRECT_ID.to_string();
        assert_eq!(
            RingCentralNotificationUser::check(&headers(&[("verification-token", &id)])),
            LegacyCheck::Valid
        );
        assert_eq!(
            RingCentralNotificationUser::check(&with_bearer),
            LegacyCheck::Rejected
        );
    }

    #[test]
    fn unchecked_providers_still_get_in() {
        assert_eq!(
            SesEventBridge::check(&HeaderMap::new()),
            LegacyCheck::Unverified
        );
        assert_eq!(
            TelnyxCallback::check(&HeaderMap::new()),
            LegacyCheck::Unverified
        );
    }
}
