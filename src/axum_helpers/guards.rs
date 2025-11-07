use crate::libs::types::BasicResponse;
use crate::posthog::{PostHogEvent, client};
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use lambda_http::tracing;
use std::env::var;
use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{Message, Recipient};
use uuid::{Uuid, uuid};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};

const CORRECT_ID: Uuid = uuid!("9ca4dfa8-0eec-46cc-967f-3385624be883");

pub struct TelegramBot {
    bot: teloxide::Bot,
}

impl TelegramBot {
    pub async fn send_message<C, T>(&self, chat: C, text: T) -> Result<Message, BasicResponse>
    where
        C: Into<Recipient>,
        T: Into<String>,
    {
        match self.bot.send_message(chat, text).await {
            Ok(message) => Ok(message),
            Err(err) => {
                tracing::error!(?err, ERR_SEND_TELEGRAM);
                Err(internal_error(ERR_SEND_TELEGRAM))
            }
        }
    }

    pub async fn edit_message_text<T>(
        &self,
        message: &MaybeInaccessibleMessage,
        text: T,
    ) -> Result<Message, BasicResponse>
    where
        T: Into<String>,
    {
        let msg_id = message.id();
        match self
            .bot
            .edit_message_text(message.chat().id, msg_id, text)
            .await
        {
            Ok(message) => Ok(message),
            Err(err) => {
                let message_id = format!("Message ID: {msg_id}");
                tracing::error!(?err, message_id = message_id, ERR_SEND_TELEGRAM);
                Err(internal_error(ERR_SEND_TELEGRAM))
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
            bot: teloxide::Bot::from_env(),
        })
    }
}

pub struct MarketingUser {
    id: uuid::Uuid,
}

impl MarketingUser {
    pub fn new(id: uuid::Uuid) -> Self {
        Self { id }
    }
}

fn parse_uuid_from_bearer(header: &str) -> Option<Uuid> {
    header
        .strip_prefix("Bearer ")
        .and_then(|token| Uuid::parse_str(token.trim()).ok())
}

async fn report_to_posthog(message: &str) {
    let api_key = match std::env::var("POSTHOG_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            tracing::error!("POSTHOG_API_KEY not set");
            return;
        }
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
        let raw_bearer = match headers.get("authorization") {
            Some(v) => v.to_str(),
            None => {
                tracing::error!("Authorization header not found");
                report_to_posthog("Authorization header not found").await;
                return Ok(MarketingUser::new(CORRECT_ID));
            }
        };
        let clean_bearer = match raw_bearer {
            Ok(bearer) => bearer,
            Err(_) => {
                tracing::error!("Failed to parse authorization header");
                report_to_posthog("Failed to parse authorization header").await;
                return Ok(MarketingUser::new(CORRECT_ID));
            }
        };

        let bearer_uuid = match parse_uuid_from_bearer(clean_bearer) {
            Some(uuid) => uuid,
            None => {
                tracing::error!("Failed to parse bearer UUID: {}", clean_bearer);
                report_to_posthog("Failed to parse bearer UUID").await;
                return Ok(MarketingUser::new(CORRECT_ID));
            }
        };

        if bearer_uuid != CORRECT_ID {
            tracing::error!("Bearer UUID does not match");
            report_to_posthog("Bearer UUID does not match").await;
            return Ok(MarketingUser::new(CORRECT_ID));
        }

        Ok(Self::new(bearer_uuid))
    }
}
