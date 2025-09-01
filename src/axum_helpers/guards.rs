use crate::libs::types::BasicResponse;
use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use lambda_http::tracing;
use std::env::var;
use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{Message, Recipient};

use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};

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
                tracing::error!(?err, message_id, ERR_SEND_TELEGRAM);
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
