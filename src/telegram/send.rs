use std::fmt::Display;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

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

pub async fn send_lead_manager_message<T: Display>(
    message: &T,
    lead_id: u64,
    user_id: i64,
    candidates: &[Candidate],
) -> Result<teloxide::prelude::Message, teloxide::RequestError> {
    let bot = Bot::from_env();
    bot.send_message(ChatId(user_id), format!("{message}. Choose a salesperson."))
        .reply_markup(kb_for_users(lead_id, candidates))
        .await
}
