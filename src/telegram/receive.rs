use crate::amazon::email::send_message;
use crate::axum_helpers::guards::TelegramBot;
use crate::crud::leads::assign_lead;
use crate::crud::leads::create_deal;
use crate::crud::users::get_user_tg_info;
use crate::crud::users::user_has_telegram_id;
use crate::crud::users::{get_user_telegram_token, set_telegram_id, set_user_telegram_token};
use crate::libs::constants::internal_error;
use crate::libs::constants::{ERR_SEND_EMAIL, OK_RESPONSE};
use crate::libs::types::BasicResponse;
use crate::telegram::utils::extract_message;
use crate::telegram::utils::parse_code;
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_slash_email};
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Update, UpdateKind};

const MESSAGE: &str = r"
Invalid message. Please send one of the following commands:
/email <email>
<code>
";

async fn handle_start_command(
    pool: &MySqlPool,
    bot: &TelegramBot,
    email: &str,
    chat_id: ChatId,
) -> BasicResponse {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        return bot
            .send_message(chat_id, "You are already registered")
            .await
            .map_or_else(
                |e| e,
                |_| (StatusCode::OK, "User already has a telegram id"),
            );
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
        tracing::error!(?e, %email, "email send failed");
        return internal_error(ERR_SEND_EMAIL);
    }

    let message =
        format!("You are now registering for {email}, please enter the code sent to your email");
    bot.send_message(chat_id, message)
        .await
        .map_or_else(|e| e, |_| OK_RESPONSE)
}

async fn handle_telegram_code(
    pool: &MySqlPool,
    bot: &TelegramBot,
    chat_id: ChatId,
    code: i32,
) -> BasicResponse {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        return bot
            .send_message(chat_id, "You are already registered")
            .await
            .map_or_else(
                |e| e,
                |_| (StatusCode::OK, "User already has a telegram id"),
            );
    }
    let db_code = get_user_telegram_token(pool, chat_id.0).await.unwrap();
    if db_code.unwrap() == code {
        set_telegram_id(pool, chat_id.0).await.unwrap();
        return bot
            .send_message(chat_id, "Accepted, you are now registered")
            .await
            .map_or_else(|e| e, |_| OK_RESPONSE);
    }
    bot.send_message(chat_id, "Invalid code")
        .await
        .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"))
}

async fn handle_message(msg: Message, pool: &MySqlPool, bot: &TelegramBot) -> BasicResponse {
    let chat_id = msg.chat.id; // ChatId
    let Some(text) = msg.text() else {
        return OK_RESPONSE;
    };

    if text.starts_with("/start") {
        let full_message = "Welcome to our bot! Please send: /email <email>";
        return bot
            .send_message(chat_id, full_message)
            .await
            .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"));
    }

    if let Some(email) = parse_slash_email(text) {
        return handle_start_command(pool, bot, &email, chat_id).await;
    }

    if let Some(code) = parse_code(text) {
        return handle_telegram_code(pool, bot, chat_id, code).await;
    }

    bot.send_message(chat_id, MESSAGE)
        .await
        .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"))
}

async fn handle_assign_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
    bot: &TelegramBot,
    cb: CallbackQuery,
) -> BasicResponse {
    let Some(message) = cb.message else {
        return (StatusCode::NOT_FOUND, "Invalid message");
    };
    assign_lead(pool, lead_id, user_id).await.unwrap();
    let tg_info = get_user_tg_info(pool, user_id).await.unwrap().unwrap();
    let former_message = extract_message(&message).unwrap_or_default();

    let user_name = tg_info.name.unwrap_or_else(|| "Unknown".to_string());
    let full_content = format!("{former_message}\n\nLead assigned to {user_name}");
    let edit_result = bot.edit_message_text(&message, full_content).await;
    if let Err(e) = edit_result {
        return e;
    }

    let result = create_deal(pool, lead_id, 1, 0, user_id).await.unwrap();
    let deal_id = result.last_insert_id();
    let lead_link = lead_url(deal_id);
    if let Some(telegram_id) = tg_info.telegram_id {
        let final_message = format!("You were assigned a lead. Click here: \n{lead_link}");
        return bot
            .send_message(ChatId(telegram_id), final_message)
            .await
            .map_or_else(|e| e, |_| (StatusCode::OK, "Invalid code"));
    }
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
    send_message(&[&tg_info.email], "Lead assigned", &message)
        .await
        .unwrap();

    OK_RESPONSE
}

async fn handle_callback(cb: CallbackQuery, pool: &MySqlPool, bot: &TelegramBot) -> BasicResponse {
    let Some(data) = &cb.data else {
        return OK_RESPONSE;
    };

    if let Some((lead_id, user_id)) = parse_assign(data) {
        return handle_assign_lead(pool, lead_id, user_id, bot, cb).await;
    }
    OK_RESPONSE
}

pub async fn webhook_handler(
    State(pool): State<MySqlPool>,
    tg_bot: TelegramBot,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> BasicResponse {
    match update.kind {
        UpdateKind::Message(msg) => handle_message(msg, &pool, &tg_bot).await,
        UpdateKind::CallbackQuery(cb) => handle_callback(cb, &pool, &tg_bot).await,
        _ => OK_RESPONSE,
    }
}
