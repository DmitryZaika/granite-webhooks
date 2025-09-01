use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use lambda_http::tracing;
use std::env::var;

use crate::libs::constants::{FORBIDDEN_RESPONSE, internal_error};

pub struct TelegramBot {
    pub bot: teloxide::Bot,
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
