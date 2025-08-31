use crate::amazon::email::send_message;
use crate::axum_helpers::guards::TelegramBot;
use crate::crud::users::{set_telegram_id, set_user_telegram_token, get_user_telegram_token};
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_start_email};
use axum::http::StatusCode;
use axum::{extract::State, response::IntoResponse};
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{ChatId, Update, UpdateKind};
use crate::crud::users::user_has_telegram_id;
use crate::telegram::utils::parse_code;

async fn handle_start_command(pool: &MySqlPool, bot: &TelegramBot, email: &str, chat_id: ChatId) -> (StatusCode, String) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot.send_message(chat_id, "You are already registered").await.unwrap();
        return (StatusCode::OK, "User already has a telegram id".to_string());
    }
    let code = gen_code();
    set_user_telegram_token(pool, chat_id.0, code, email).await.unwrap();
   let message_result = send_message(
        &[&email],
        "Graninte Manager Code",
        &format!("Your code is: {code}"),
    )
    .await;
    if let Err(e) = message_result {
        let format_error = format!("Error 1: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, format_error);
    }

    let message =format!(
        "You are now registering for {email}, please enter the code sent to your email"
    );
    let result = bot.bot.send_message(chat_id, message).await;
    if let Err(e) = result {
        let format_error = format!("Error 2: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, format_error);
    }

    return (StatusCode::OK, "ok".to_string());
}

async fn handle_telegram_code(pool: &MySqlPool, bot: &TelegramBot, chat_id: ChatId, code: i32) -> (StatusCode, String) {
    if user_has_telegram_id(pool, chat_id.0).await.unwrap() {
        bot.bot.send_message(chat_id, "You are already registered").await.unwrap();
        return (StatusCode::OK, "User already has a telegram id".to_string());
    }
   let db_code = get_user_telegram_token(pool, chat_id.0).await.unwrap();
   if db_code.unwrap() == code {
    set_telegram_id(pool, chat_id.0).await.unwrap();
    bot.bot.send_message(chat_id, "Accepted, you are now registered").await.unwrap();
        return (StatusCode::OK, "ok".to_string());
   } 
   bot.bot.send_message(chat_id, "Invalid code").await.unwrap();
   return (StatusCode::OK, "ok".to_string());
}

async fn handle_message(
    msg: Message,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> (StatusCode, String) {
    let chat_id = msg.chat.id; // ChatId
    let text = match msg.text() {
        Some(text) => text,
        None => return (StatusCode::OK, "ok".to_string())
    };

    if let Some(email) = parse_start_email(text) {
        return handle_start_command(pool, bot, &email, chat_id).await;
    }

    if let Some(code) = parse_code(text) {
        return handle_telegram_code(pool, bot, chat_id, code).await;
    }

    const MESSAGE: &str = r#"
    Invalid message. Please send one of the following commands:
    /start <email>
    <code>
    "#;
    bot.bot.send_message(chat_id, MESSAGE).await.unwrap();
    (StatusCode::OK, "ok".to_string())
}

async fn handle_button(
    cb: CallbackQuery,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> Option<(StatusCode, String)> {
    println!("handle_button: {cb:?}");
    let bot = bot.bot.clone();
    let data = cb.data?;
    // A) Resend-кнопка после 3 неудачных попыток
    if let Some(email) = data.strip_prefix("resend:") {
        let chat_id = if let Some(msg) = &cb.message {
            msg.chat().id
        } else {
            // fallback: личный чат
            ChatId(cb.from.id.0 as i64)
        };

        let chat_i64 = chat_id.0;

        // генерим новый код, сбрасываем попытки
        let code = gen_code();

        // ACK
        let _ = bot.answer_callback_query(cb.id.clone()).await;

        // отправка письма
        send_message(
            &[email],
            &format!("Your code is: {code}"),
            &format!("Your code is: {code}"),
        )
        .await
        .unwrap();

        // уведомляем
        let result = bot
            .send_message(
                chat_id,
                format!("A new code was sent to {email}. Please enter it here."),
            )
            .await;
        if let Err(e) = result {
            return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }

        // можно также отредактировать исходное сообщение (если есть)
        let msg = cb.message?;
        let _ = bot
            .edit_message_text(
                msg.chat().id,
                msg.id(),
                "New code sent. Please enter it below.",
            )
            .await;

        return Some((StatusCode::OK, "ok".to_string()));
    }

    // B) Твоя существующая логика assign:lead_id:user_id
    if let Some((lead_id, user_chat_id)) = parse_assign(&data) {
        // ACK
        let _ = bot.answer_callback_query(cb.id.clone()).await;

        let lead_link = lead_url(lead_id);
        let result = bot
            .send_message(
                ChatId(user_chat_id),
                format!("You were assigned a lead. Click here: \n#{lead_link}"),
            )
            .await;
        if let Err(e) = result {
            return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }

        let msg = cb.message?;
        let _ = bot
            .edit_message_text(
                msg.chat().id,
                msg.id(),
                format!("Lead #{lead_id} assigned to {user_chat_id}"),
            )
            .await;
    }
    None
}

pub async fn webhook_handler(
    State(pool): State<MySqlPool>,
    tg_bot: TelegramBot,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let result = match update.kind {
        UpdateKind::Message(msg) => Some(handle_message(msg, &pool, &tg_bot).await),
        UpdateKind::CallbackQuery(cb) => handle_button(cb, &pool, &tg_bot).await,
        _ => None,
    };
    if let Some(response) = result {
        return Ok(response);
    }

    Ok((StatusCode::OK, "ok".to_string()))
}
