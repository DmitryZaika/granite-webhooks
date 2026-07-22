use common::telegram::crm::{
    format_activity_notification, format_email_notification,
};

use crate::axum_helpers::guards::Telegram;
use crate::crud::users::get_user_tg_info;
use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;
use lambda_http::tracing;
use sqlx::MySqlPool;
use teloxide::prelude::*;

pub struct CrmTelegramNotify {
    pub user_id: i32,
    pub deal_id: i32,
    pub notification_type: String,
    pub message: String,
    pub actor_name: Option<String>,
    pub customer_name: Option<String>,
}

pub struct InboundEmailTelegramNotify {
    pub receiver_user_id: i32,
    pub thread_id: String,
    pub subject: Option<String>,
    pub deal_id: Option<u64>,
    pub customer_name: Option<String>,
}

pub async fn send_crm_telegram_notification<T>(
    pool: &MySqlPool,
    bot: &T,
    payload: &CrmTelegramNotify,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let user = match get_user_tg_info(pool, payload.user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(?error, user_id = payload.user_id, "Failed to load user telegram info");
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
    };
    let Some(user) = user else {
        return Ok(());
    };
    let Some(telegram_id) = user.telegram_id else {
        return Ok(());
    };
    if !user.telegram_activity_notifications {
        return Ok(());
    }

    let text = format_activity_notification(
        &payload.notification_type,
        payload.customer_name.as_deref(),
        payload.actor_name.as_deref(),
        &payload.message,
        payload.deal_id,
    );
    send_plain_crm_message(bot, telegram_id, &text).await
}

pub async fn send_inbound_email_telegram_notification<T>(
    bot: &T,
    payload: &InboundEmailTelegramNotify,
    telegram_id: i64,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let text = format_email_notification(
        payload.customer_name.as_deref(),
        payload.subject.as_deref(),
        payload.deal_id,
        &payload.thread_id,
    );
    send_plain_crm_message(bot, telegram_id, &text).await
}

pub async fn send_deadline_reminder_telegram<T>(
    bot: &T,
    telegram_id: i64,
    customer_name: Option<&str>,
    message: &str,
    deal_id: i32,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let text = format_activity_notification(
        "activity_deadline_reminder",
        customer_name,
        None,
        message,
        deal_id,
    );
    send_plain_crm_message(bot, telegram_id, &text).await
}

async fn send_plain_crm_message<T>(
    bot: &T,
    telegram_id: i64,
    text: &str,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    match bot.send_message(ChatId(telegram_id), text.to_string()).await {
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::error!(
                ?error,
                telegram_id = telegram_id,
                "Failed to send CRM telegram notification"
            );
            Err(internal_error(ERR_SEND_TELEGRAM))
        }
    }
}
