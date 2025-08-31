use crate::amazon::email::send_message;
use crate::axum_helpers::guards::TelegramBot;
use crate::crud::leads::assign_lead;
use crate::crud::leads::create_deal;
use crate::crud::users::get_user_tg_info;
use crate::crud::users::user_has_telegram_id;
use crate::crud::users::{get_user_telegram_token, set_telegram_id, set_user_telegram_token};
use crate::telegram::utils::parse_code;
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_start_email};
use axum::extract::State;
use axum::http::StatusCode;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{ChatId, Update, UpdateKind};

async fn handle_start_command(
    pool: &MySqlPool,
    bot: &TelegramBot,
    email: &str,
    chat_id: ChatId,
) -> (StatusCode, String) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot
            .send_message(chat_id, "You are already registered")
            .await
            .unwrap();
        return (StatusCode::OK, "User already has a telegram id".to_string());
    }
    let code = gen_code();
    set_user_telegram_token(pool, chat_id.0, code, email)
        .await
        .unwrap();
    let message_result = send_message(
        &[email],
        "Graninte Manager Code",
        &format!("Your code is: {code}"),
    )
    .await;
    if let Err(e) = message_result {
        let format_error = format!("Error 1: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, format_error);
    }

    let message =
        format!("You are now registering for {email}, please enter the code sent to your email");
    let result = bot.bot.send_message(chat_id, message).await;
    if let Err(e) = result {
        let format_error = format!("Error 2: {e}");
        return (StatusCode::INTERNAL_SERVER_ERROR, format_error);
    }

    (StatusCode::OK, "ok".to_string())
}

async fn handle_telegram_code(
    pool: &MySqlPool,
    bot: &TelegramBot,
    chat_id: ChatId,
    code: i32,
) -> (StatusCode, String) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot
            .send_message(chat_id, "You are already registered")
            .await
            .unwrap();
        return (StatusCode::OK, "User already has a telegram id".to_string());
    }
    let db_code = get_user_telegram_token(pool, chat_id.0).await.unwrap();
    if db_code.unwrap() == code {
        set_telegram_id(pool, chat_id.0).await.unwrap();
        bot.bot
            .send_message(chat_id, "Accepted, you are now registered")
            .await
            .unwrap();
        return (StatusCode::OK, "ok".to_string());
    }
    bot.bot.send_message(chat_id, "Invalid code").await.unwrap();
    (StatusCode::OK, "ok".to_string())
}

async fn handle_message(msg: Message, pool: &MySqlPool, bot: &TelegramBot) -> (StatusCode, String) {
    let chat_id = msg.chat.id; // ChatId
    let text = match msg.text() {
        Some(text) => text,
        None => return (StatusCode::OK, "ok".to_string()),
    };

    if let Some(email) = parse_start_email(text) {
        return handle_start_command(pool, bot, &email, chat_id).await;
    }

    if let Some(code) = parse_code(text) {
        return handle_telegram_code(pool, bot, chat_id, code).await;
    }

    const MESSAGE: &str = r"
    Invalid message. Please send one of the following commands:
    /start <email>
    <code>
    ";
    match bot.bot.send_message(chat_id, MESSAGE).await {
        Ok(_) => (StatusCode::OK, "ok".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn handle_assign_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
    bot: &TelegramBot,
    cb: CallbackQuery,
) -> (StatusCode, String) {
    // let _ = bot.bot.answer_callback_query(cb.id.clone()).await;
    assign_lead(pool, lead_id, user_id).await.unwrap();
    let tg_info = get_user_tg_info(pool, user_id).await.unwrap().unwrap();
    let mut former_message = "Unknown";
    let msg = cb.message.unwrap();
    if let MaybeInaccessibleMessage::Regular(msg) = &msg
        && let Some(text) = msg.text()
    {
        former_message = text;
    }

    let user_name = tg_info.name.unwrap_or("Unknown".to_string());
    let full_content = format!("{former_message}\n\nLead assigned to {user_name}");
    bot.bot
        .edit_message_text(msg.chat().id, msg.id(), full_content)
        .await
        .unwrap();

    let result = create_deal(pool, lead_id, 1, 0, user_id).await.unwrap();
    let deal_id = result.last_insert_id();
    let lead_link = lead_url(deal_id);
    if let Some(telegram_id) = tg_info.telegram_id {
        let result = bot
            .bot
            .send_message(
                ChatId(telegram_id),
                format!("You were assigned a lead. Click here: \n{lead_link}"),
            )
            .await;
        if let Err(e) = result {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    } else {
        let bot_link = "https://t.me/granitemanager_bot?start";
        let message = format!(
            r"
    You were assigned a lead. Click here:
    {lead_link}
    
    Please link to telegram bot:
    {bot_link}

    Paste this command into the bot: \start {}
    ",
            tg_info.email
        );
        send_message(&[&tg_info.email], "Lead assigned", &message).await.unwrap();
    }

    (StatusCode::OK, "ok".to_string())
}

async fn handle_callback(
    cb: CallbackQuery,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> (StatusCode, String) {
    let data = match &cb.data {
        Some(data) => data,
        None => return (StatusCode::OK, "ok".to_string()),
    };

    if let Some((lead_id, user_id)) = parse_assign(data) {
        return handle_assign_lead(pool, lead_id, user_id, bot, cb).await;
    }
    (StatusCode::OK, "ok".to_string())
}

pub async fn webhook_handler(
    State(pool): State<MySqlPool>,
    tg_bot: TelegramBot,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> (StatusCode, String) {
    match update.kind {
        UpdateKind::Message(msg) => handle_message(msg, &pool, &tg_bot).await,
        UpdateKind::CallbackQuery(cb) => handle_callback(cb, &pool, &tg_bot).await,
        _ => (StatusCode::OK, "ok".to_string()),
    }
}
