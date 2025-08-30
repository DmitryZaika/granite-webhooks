use crate::amazon::email::send_message;
use crate::axum_helpers::guards::TelegramBot;
use crate::crud::users::set_telegram_id;
use crate::telegram::utils::{gen_code, lead_url, parse_assign, parse_start_email};
use axum::http::StatusCode;
use axum::{extract::State, response::IntoResponse};
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Update, UpdateKind};

fn new_resend_keyboard(email: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![InlineKeyboardButton::callback(
        "Send new code",
        format!("resend:{email}"),
    )]])
}

async fn handle_start_command(
    msg: Message,
    pool: &MySqlPool,
    bot: &TelegramBot,
) -> Option<(StatusCode, String)> {
    let chat_id = msg.chat.id; // ChatId
    let chat_i64 = chat_id.0;
    let text = msg.text()?;

    // 1) /start <email> — регистрируемся, шлём код
    if let Some(email) = parse_start_email(text) {
        // генерим код
        let code = gen_code();

        // сохраняем состояние (3 попытки)

        // отправляем письмо (заглушка/реальная интеграция)
        send_message(
            &[&email],
            "Graninte Manager Code",
            &format!("Your code is: {code}"),
        )
        .await
        .unwrap();

        // пишем пользователю
        let result = bot
            .bot
            .send_message(
                chat_id,
                format!(
                    "You are now registering for {email}, please enter the code sent to your email"
                ),
            )
            .await;
        if let Err(e) = result {
            return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }

        return Some((StatusCode::OK, "ok".to_string()));
    }

    // 2) Если есть незавершённая верификация — пробуем интерпретировать текст как код
    let code = "123456";
    let email = "colin@gmail.com";
    let user_code = text.trim();
    if user_code.trim() == code.trim() {
        // успех
        let email = email;
        // можно здесь записать в БД связь chat_id <-> email
        // ctx.verifications.remove(&chat_i64);
        set_telegram_id(pool, email, &chat_i64.to_string())
            .await
            .unwrap();
        let result = bot
            .bot
            .send_message(chat_id, "Accepted, you are now registered")
            .await;
        if let Err(e) = result {
            return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    } 
    None
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
        UpdateKind::Message(msg) => handle_start_command(msg, &pool, &tg_bot).await,
        UpdateKind::CallbackQuery(cb) => handle_button(cb, &pool, &tg_bot).await,
        _ => None,
    };
    if let Some(response) = result {
        return Ok(response);
    }

    Ok((StatusCode::OK, "ok".to_string()))
}
