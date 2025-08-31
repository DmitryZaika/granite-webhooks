use axum::extract::FromRequestParts;
use axum::http::{StatusCode, request::Parts};
use std::env::var;

pub struct TelegramBot {
    pub bot: teloxide::Bot,
}

impl<S> FromRequestParts<S> for TelegramBot
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let webhook_secret = var("WEBHOOK_SECRET").expect("WEBHOOK_SECRET must be set");
        let secret = parts
            .headers
            .get("x-telegram-bot-api-secret-token")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| (StatusCode::FORBIDDEN, "forbidden".to_string()))?;

        if secret != webhook_secret {
            return Err((StatusCode::FORBIDDEN, "forbidden".to_string()));
        }
        Ok(Self {
            bot: teloxide::Bot::from_env(),
        })
    }
}


