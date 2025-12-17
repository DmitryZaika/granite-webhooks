use std::fmt::Display;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::crud::users::get_sales_users;
use crate::libs::constants::{ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;

use lambda_http::tracing;
use sqlx::MySqlPool;

pub const fn documenso() -> BasicResponse {
    OK_RESPONSE
}

type Candidate = (
    String, /*name*/
    i32,    /*tg_chat_id*/
    i64,    /*mtd_lead_count*/
);

fn kb_for_users(lead_id: u64, candidates: &[Candidate]) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for chunk in candidates.chunks(2) {
        let mut row: Vec<InlineKeyboardButton> = Vec::new();
        for (name, user_id, mtd_lead_count) in chunk {
            row.push(InlineKeyboardButton::callback(
                format!("{}: {}", name.clone(), mtd_lead_count),
                format!("assign:{lead_id}:{user_id}"),
            ));
        }
        rows.push(row);
    }

    InlineKeyboardMarkup::new(rows)
}

pub async fn send_lead_manager_message<T>(
    message: &T,
    lead_id: u64,
    user_id: i64,
    candidates: &[Candidate],
) -> Result<teloxide::prelude::Message, teloxide::RequestError>
where
    T: Display + Sync + ?Sized,
{
    let bot = Bot::from_env();
    bot.send_message(ChatId(user_id), format!("{message}. Choose a salesperson."))
        .reply_markup(kb_for_users(lead_id, candidates))
        .await
}

pub async fn send_plain_message_to_chat(
    chat_id: i64,
    message: &str,
) -> Result<teloxide::prelude::Message, teloxide::RequestError> {
    let bot = Bot::from_env();
    bot.send_message(ChatId(chat_id), message.to_string()).await
}

pub async fn send_telegram_manager_assign<T: Display>(
    pool: &MySqlPool,
    company_id: i32,
    data: T,
    customer_id: u64,
) -> Result<(), BasicResponse> {
    let all_users = match get_sales_users(pool, company_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Error fetching users");
            return Err(internal_error(ERR_DB));
        }
    };
    let candidates: Vec<(String, i32, i64)> = all_users
        .iter()
        .filter(|item| item.position_id == Some(1))
        .map(|user| {
            (
                user.name.clone().unwrap_or_else(|| "Unknown".to_string()),
                user.id,
                user.mtd_lead_count,
            )
        })
        .collect();
    if let Some(telegram_id) = all_users
        .iter()
        .find(|u| u.position_id == Some(2))
        .and_then(|u| u.telegram_id)
    {
        let send_message =
            send_lead_manager_message(&data.to_string(), customer_id, telegram_id, &candidates)
                .await;

        if send_message.is_err() {
            tracing::error!(
                ?send_message,
                telegram_id = telegram_id,
                "Error sending message to lead manager"
            );
        }
    }

    Ok(())
}

pub async fn send_telegram_duplicate_notification(
    pool: &MySqlPool,
    company_id: i32,
    lead_name: &str,
    assigned_id: i32,
    lead_body: String,
) -> bool {
    let all_users = match get_sales_users(pool, company_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Error fetching users");
            return false;
        }
    };
    let assigned_name = all_users.iter().find(|u| u.id == assigned_id).map_or_else(
        || "Unknown".to_string(),
        |u| u.name.clone().unwrap_or_else(|| "Unknown".to_string()),
    );
    if let Some(telegram_id) = all_users
        .iter()
        .find(|u| u.position_id == Some(2))
        .and_then(|u| u.telegram_id)
    {
        let message =
            format!("Repeat lead {lead_name} with for sales rep {assigned_name}\n\n{lead_body}");
        let response = send_plain_message_to_chat(telegram_id, &message).await;
        if response.is_err() {
            tracing::error!(
                ?message,
                telegram_id = telegram_id,
                "Error sending message to lead manager"
            );
        }
    }
    true
}
