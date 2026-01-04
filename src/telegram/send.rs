use crate::axum_helpers::guards::Telegram;
use std::fmt::Display;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tokio::task::JoinSet;

use crate::crud::users::{SalesUser, get_sales_users};
use crate::libs::constants::{ERR_DB, OK_RESPONSE, SALES_MANAGER, SALES_WORKER, internal_error};
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

pub async fn send_lead_manager_message_to_all<T, V>(
    message: &T,
    lead_id: u64,
    telegram_ids: Vec<i64>,
    candidates: &[Candidate],
    raw_bot: Arc<V>,
) -> Result<Vec<Message>, teloxide::RequestError>
where
    T: Display + Sync + ?Sized,
    V: Telegram + Send + Sync + 'static + Clone,
{
    let full_message = format!("{message}. Choose a salesperson.");
    let kb = kb_for_users(lead_id, candidates);

    let mut set = JoinSet::new();

    for user_id in telegram_ids {
        let bot = Arc::clone(&raw_bot);
        let msg = full_message.clone();
        let kb = kb.clone();

        set.spawn(async move { bot.send_repliable_message(ChatId(user_id), msg, kb).await });
    }

    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        let msg = res.expect("task panicked or was cancelled")?;
        out.push(msg);
    }

    Ok(out)
}

pub async fn send_plain_message_to_chat<T>(
    chat_id: i64,
    message: &str,
    bot: &T,
) -> Result<teloxide::prelude::Message, BasicResponse>
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    bot.send_message(ChatId(chat_id), message.to_string()).await
}

fn get_manager_telegram_ids(users: &[SalesUser]) -> Vec<i64> {
    users
        .iter()
        .filter(|u| u.position_id == SALES_MANAGER)
        .filter_map(|u| u.telegram_id)
        .collect()
}

pub async fn send_telegram_manager_assign<T: Display, V>(
    pool: &MySqlPool,
    company_id: i32,
    data: T,
    customer_id: u64,
    bot: &V,
) -> Result<(), BasicResponse>
where
    V: Telegram + Send + Sync + 'static + Clone,
{
    let all_users = match get_sales_users(pool, company_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Error fetching users");
            return Err(internal_error(ERR_DB));
        }
    };
    let candidates: Vec<(String, i32, i64)> = all_users
        .iter()
        .filter(|item| item.position_id == SALES_WORKER)
        .map(|user| {
            (
                user.name.clone().unwrap_or_else(|| "Unknown".to_string()),
                user.id,
                user.mtd_lead_count,
            )
        })
        .collect();
    let telegram_ids = get_manager_telegram_ids(&all_users);

    if telegram_ids.is_empty() {
        tracing::error!(
            ?company_id,
            position_id = SALES_MANAGER,
            "No sales manager found"
        );
        return Err(internal_error(ERR_DB));
    }
    let new_bot = Arc::new(bot.clone());
    let send_message = send_lead_manager_message_to_all(
        &data.to_string(),
        customer_id,
        telegram_ids.clone(),
        &candidates,
        new_bot,
    )
    .await;

    if send_message.is_err() {
        let telegram_ids_str = telegram_ids
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        tracing::error!(
            ?send_message,
            telegram_ids = %telegram_ids_str,
            "Error sending message to lead manager"
        );
    }

    Ok(())
}

pub async fn send_lead_managers_dupliacate<V>(
    message: String,
    telegram_ids: Vec<i64>,
    raw_bot: Arc<V>,
) -> Result<Vec<Message>, teloxide::RequestError>
where
    V: Telegram + Send + Sync + 'static + Clone,
{
    let full_message = format!("{message}. Choose a salesperson.");

    let mut set = JoinSet::new();

    for user_id in telegram_ids.clone() {
        let bot = Arc::clone(&raw_bot);
        let msg = full_message.clone();

        set.spawn(async move { send_plain_message_to_chat(user_id, &msg, bot.as_ref()).await });
    }

    let mut out = Vec::new();
    while let Some(res) = set.join_next().await {
        let res_inner = match res {
            Ok(msg) => msg,
            Err(error) => {
                tracing::error!(
                    ?error,
                    ?message,
                    ?telegram_ids,
                    "Error sending message to lead manager"
                );
                continue;
            }
        };
        match res_inner {
            Ok(msg) => out.push(msg),
            Err(error) => {
                tracing::error!(
                    ?error,
                    ?message,
                    ?telegram_ids,
                    "Error sending message to lead manager",
                );
            }
        }
    }

    Ok(out)
}

pub async fn send_telegram_duplicate_notification<T>(
    pool: &MySqlPool,
    company_id: i32,
    lead_name: &str,
    assigned_id: i32,
    lead_body: String,
    bot: &T,
) -> bool
where
    T: Telegram + Send + Sync + 'static + Clone,
{
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
    let telegram_ids = get_manager_telegram_ids(&all_users);
    if telegram_ids.is_empty() {
        tracing::error!(
            ?company_id,
            position_id = SALES_MANAGER,
            "No sales manager found"
        );
        return false;
    }
    let message =
        format!("Repeat lead {lead_name} with for sales rep {assigned_name}\n\n{lead_body}");
    let new_bot = Arc::new(bot.clone());
    send_lead_managers_dupliacate(message, telegram_ids, new_bot)
        .await
        .is_err()
}
