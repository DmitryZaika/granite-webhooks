use crate::axum_helpers::guards::{NotificationsTelegramBot, RemixBackend, Telegram};
use crate::crud::users::get_user_notifications_tg_info;
use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::OK_RESPONSE;
use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::MySqlPool;
use teloxide::prelude::*;

#[derive(Debug, Deserialize)]
pub struct NotificationsNotifyRequest {
    pub user_id: i32,
    pub message: String,
}

pub async fn notifications_notify_handler(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Json(body): Json<NotificationsNotifyRequest>,
) -> BasicResponse {
    let bot = NotificationsTelegramBot::default();
    if let Err(error) = send_notifications_telegram_message(&pool, &bot, body.user_id, &body.message)
        .await
    {
        tracing::error!(
            ?error,
            user_id = body.user_id,
            "Notifications telegram notify failed"
        );
    }
    OK_RESPONSE
}

async fn send_notifications_telegram_message<T>(
    pool: &MySqlPool,
    bot: &T,
    user_id: i32,
    message: &str,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let user = match get_user_notifications_tg_info(pool, user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                ?error,
                user_id = user_id,
                "Failed to load user notifications telegram info"
            );
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
    };
    let Some(user) = user else {
        return Ok(());
    };
    let Some(telegram_id) = user.notifications_telegram_id else {
        return Ok(());
    };

    match bot
        .send_message(ChatId(telegram_id), message.to_string())
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::error!(
                ?error,
                telegram_id = telegram_id,
                "Failed to send notifications telegram message"
            );
            Err(internal_error(ERR_SEND_TELEGRAM))
        }
    }
}
