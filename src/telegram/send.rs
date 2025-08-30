use std::fmt::Display;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

fn kb_for_users(
    lead_id: u64,
    candidates: &[(String /*name*/, i32 /*tg_chat_id*/)],
) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    for chunk in candidates.chunks(2) {
        let mut row: Vec<InlineKeyboardButton> = Vec::new();
        for (name, chat_id) in chunk {
            row.push(InlineKeyboardButton::callback(
                name.clone(),
                format!("assign:{lead}:{user}", lead = lead_id, user = chat_id),
            ));
        }
        rows.push(row);
    }

    InlineKeyboardMarkup::new(rows)
}



pub async fn send_lead_manager_message<T: Display>(
    message: &T,
    lead_id: u64,
    user_id: i64,
    candidates: &[(String /*name*/, i32 /*tg_chat_id*/)],
) -> Result<teloxide::prelude::Message, teloxide::RequestError>{
    let bot = Bot::from_env();
    bot.send_message(
        ChatId(user_id),
        format!("{message}. Choose a salesperson."),
    )
    .reply_markup(kb_for_users(lead_id, &candidates))
    .await
}
