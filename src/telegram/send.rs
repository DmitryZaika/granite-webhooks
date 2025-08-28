use std::fmt::Display;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

fn kb_for_users(
    lead_id: i64,
    candidates: &[(String /*name*/, i64 /*tg_chat_id*/)],
) -> InlineKeyboardMarkup {
    let row: Vec<InlineKeyboardButton> = candidates
        .iter()
        .map(|(name, chat_id)| {
            // В callback_data кладём короткий токен/ID (ограничение Telegram ~64 байта)
            InlineKeyboardButton::callback(
                name.clone(),
                format!("assign:{lead}:{user}", lead = lead_id, user = chat_id),
            )
        })
        .collect();
    InlineKeyboardMarkup::new(vec![row])
}

pub async fn send_lead_manager_message<T: Display>(
    message: &T,
    lead_id: i64,
    user_id: i64,
    candidates: &[(String /*name*/, i64 /*tg_chat_id*/)],
) {
    let bot = Bot::from_env();
    bot.send_message(
        ChatId(user_id),
        format!("{}. Choose a salesperson.", message),
    )
    .reply_markup(kb_for_users(lead_id, &candidates))
    .await;
}
