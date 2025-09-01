use crate::amazon::email::send_message;
use crate::axum_helpers::guards::TelegramBot;
use crate::crud::leads::assign_lead;
use crate::crud::leads::create_deal;
use crate::crud::users::get_user_tg_info;
use crate::crud::users::user_has_telegram_id;
use crate::crud::users::{get_user_telegram_token, set_telegram_id, set_user_telegram_token};
use crate::libs::constants::OK_RESPONSE;
use crate::telegram::utils::parse_code;
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_slash_email};
use axum::extract::State;
use axum::http::StatusCode;
use lambda_http::tracing;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::MaybeInaccessibleMessage;
use teloxide::types::{ChatId, Update, UpdateKind};

const MESSAGE: &str = r"
Invalid message. Please send one of the following commands:
/email <email>
<code>
";

const ERR_SEND_EMAIL: &str = "send_email_failed";
const ERR_SEND_TELEGRAM: &str = "telegram_send_failed";

async fn handle_start_command(
    pool: &MySqlPool,
    bot: &TelegramBot,
    email: &str,
    chat_id: ChatId,
) -> (StatusCode, &'static str) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot
            .send_message(chat_id, "You are already registered")
            .await
            .unwrap();
        return OK_RESPONSE;
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
        return (StatusCode::INTERNAL_SERVER_ERROR, ERR_SEND_EMAIL);
    }

    let message =
        format!("You are now registering for {email}, please enter the code sent to your email");
    let result = bot.bot.send_message(chat_id, message).await;
    if let Err(e) = result {
        tracing::error!(?e, chat_id = chat_id.0, "telegram send failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, ERR_SEND_TELEGRAM);
    }

    OK_RESPONSE
}

async fn handle_telegram_code(
    pool: &MySqlPool,
    bot: &TelegramBot,
    chat_id: ChatId,
    code: i32,
) -> (StatusCode, &'static str) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot
            .send_message(chat_id, "You are already registered")
            .await
            .unwrap();
        return (StatusCode::OK, "User already has a telegram id");
    }
    let db_code = get_user_telegram_token(pool, chat_id.0).await.unwrap();
    if db_code.unwrap() == code {
        set_telegram_id(pool, chat_id.0).await.unwrap();
        bot.bot
            .send_message(chat_id, "Accepted, you are now registered")
            .await
            .unwrap();
        return OK_RESPONSE;
    }
    bot.bot.send_message(chat_id, "Invalid code").await.unwrap();
    OK_RESPONSE
}

async fn handle_message(
    msg: Message,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> (StatusCode, &'static str) {
    let chat_id = msg.chat.id; // ChatId
    let text = match msg.text() {
        Some(text) => text,
        None => return OK_RESPONSE,
    };

    if text.starts_with("/start") {
        let full_message = "Welcome to our bot! Please send: /email <email>";
        bot.bot.send_message(chat_id, full_message).await.unwrap();
    }

    if let Some(email) = parse_slash_email(text) {
        return handle_start_command(pool, bot, &email, chat_id).await;
    }

    if let Some(code) = parse_code(text) {
        return handle_telegram_code(pool, bot, chat_id, code).await;
    }

    match bot.bot.send_message(chat_id, MESSAGE).await {
        Ok(_) => OK_RESPONSE,
        Err(e) => {
            tracing::error!(?e, %chat_id, "failed to send telegram");
            return (StatusCode::INTERNAL_SERVER_ERROR, ERR_SEND_TELEGRAM);
        }
    }
}

async fn handle_assign_lead(
    pool: &MySqlPool,
    lead_id: i32,
    user_id: i64,
    bot: &TelegramBot,
    cb: CallbackQuery,
) -> (StatusCode, &'static str) {
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
            tracing::error!(?e, %telegram_id, "failed to send telegram");
            return (StatusCode::INTERNAL_SERVER_ERROR, ERR_SEND_TELEGRAM);
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
        send_message(&[&tg_info.email], "Lead assigned", &message)
            .await
            .unwrap();
    }

    OK_RESPONSE
}

async fn handle_callback(
    cb: CallbackQuery,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> (StatusCode, &'static str) {
    let data = match &cb.data {
        Some(data) => data,
        None => return OK_RESPONSE,
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
) -> (StatusCode, &'static str) {
    match update.kind {
        UpdateKind::Message(msg) => handle_message(msg, &pool, &tg_bot).await,
        UpdateKind::CallbackQuery(cb) => handle_callback(cb, &pool, &tg_bot).await,
        _ => OK_RESPONSE,
    }
}
