use common::telegram::crm::{
    format_activity_notification, format_email_notification, format_sms_notification,
};

use crate::axum_helpers::guards::Telegram;
use crate::crud::users::get_user_notifications_tg_info;
use crate::libs::constants::ERR_SEND_TELEGRAM;
use crate::libs::constants::internal_error;
use crate::libs::types::BasicResponse;
use lambda_http::tracing;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

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
    pub body: Option<String>,
    pub deal_id: Option<u64>,
    pub customer_name: Option<String>,
}

pub struct InboundSmsTelegramNotify {
    pub receiver_user_id: i32,
    pub sender_phone: String,
    pub message: String,
}

pub async fn send_crm_telegram_notification<T>(
    pool: &MySqlPool,
    bot: &T,
    payload: &CrmTelegramNotify,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let user = match get_user_notifications_tg_info(pool, payload.user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                ?error,
                user_id = payload.user_id,
                "Failed to load user telegram info"
            );
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
    };
    let Some(user) = user else {
        return Ok(());
    };
    if !user.telegram_activity_notifications {
        return Ok(());
    }
    let Some(telegram_id) = user.notifications_telegram_id else {
        return Ok(());
    };

    let message = format_activity_notification(
        &payload.notification_type,
        payload.customer_name.as_deref(),
        payload.actor_name.as_deref(),
        &payload.message,
        payload.deal_id,
    );
    send_crm_message_with_button(
        bot,
        telegram_id,
        &message.text,
        message.button_label,
        &message.button_url,
        None,
    )
    .await
}

pub async fn send_inbound_email_telegram_notification<T>(
    pool: &MySqlPool,
    bot: &T,
    payload: &InboundEmailTelegramNotify,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let user = match get_user_notifications_tg_info(pool, payload.receiver_user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                ?error,
                receiver_user_id = payload.receiver_user_id,
                "Failed to load receiver telegram info for inbound email"
            );
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
    };
    let Some(user) = user else {
        return Ok(());
    };
    if !user.telegram_email_notifications {
        return Ok(());
    }
    let Some(telegram_id) = user.notifications_telegram_id else {
        return Ok(());
    };

    let message = format_email_notification(
        payload.customer_name.as_deref(),
        payload.subject.as_deref(),
        payload.body.as_deref(),
        payload.deal_id,
        &payload.thread_id,
    );
    send_crm_message_with_button(
        bot,
        telegram_id,
        &message.text,
        message.button_label,
        &message.button_url,
        Some(ParseMode::Html),
    )
    .await
}

pub async fn send_inbound_sms_telegram_notification<T>(
    pool: &MySqlPool,
    bot: &T,
    payload: &InboundSmsTelegramNotify,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let user = match get_user_notifications_tg_info(pool, payload.receiver_user_id).await {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                ?error,
                receiver_user_id = payload.receiver_user_id,
                "Failed to load receiver telegram info for inbound sms"
            );
            return Err(internal_error(ERR_SEND_TELEGRAM));
        }
    };
    let Some(user) = user else {
        return Ok(());
    };
    if !user.telegram_sms_notifications {
        return Ok(());
    }
    let Some(telegram_id) = user.notifications_telegram_id else {
        return Ok(());
    };

    let phone_digits: String = payload
        .sender_phone
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect();
    let message = format_sms_notification(&payload.sender_phone, &payload.message, &phone_digits);
    send_crm_message_with_button(
        bot,
        telegram_id,
        &message.text,
        message.button_label,
        &message.button_url,
        None,
    )
    .await
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
    let notification = format_activity_notification(
        "activity_deadline_reminder",
        customer_name,
        None,
        message,
        deal_id,
    );
    send_crm_message_with_button(
        bot,
        telegram_id,
        &notification.text,
        notification.button_label,
        &notification.button_url,
        None,
    )
    .await
}

fn open_url_keyboard(label: &str, url: &str) -> Result<InlineKeyboardMarkup, BasicResponse> {
    let parsed = url.parse::<reqwest::Url>().map_err(|error| {
        tracing::error!(?error, url, "Invalid CRM telegram button url");
        internal_error(ERR_SEND_TELEGRAM)
    })?;
    Ok(InlineKeyboardMarkup::new([[InlineKeyboardButton::url(
        label.to_string(),
        parsed,
    )]]))
}

async fn send_crm_message_with_button<T>(
    bot: &T,
    telegram_id: i64,
    text: &str,
    button_label: &str,
    button_url: &str,
    parse_mode: Option<ParseMode>,
) -> Result<(), BasicResponse>
where
    T: Telegram + Send + Sync,
{
    let keyboard = open_url_keyboard(button_label, button_url)?;
    match bot
        .send_repliable_message(
            ChatId(telegram_id),
            text.to_string(),
            keyboard,
            parse_mode,
        )
        .await
    {
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
