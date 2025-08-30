use crate::amazon::email::send_message;
use crate::crud::users::set_telegram_id;
use crate::schemas::state::{AppState, VerificationState};
use axum::http::{StatusCode, header::HeaderMap};
use axum::{extract::State, response::IntoResponse};
use rand::Rng;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Update, UpdateKind};

fn parse_assign(data: &str) -> Option<(i32, i64)> {
    let parts: Vec<&str> = data.split(':').collect();
    if parts.len() == 3 && parts[0] == "assign" {
        let lead_id = parts[1].parse().ok()?;
        let user_id = parts[2].parse().ok()?;
        Some((lead_id, user_id))
    } else {
        None
    }
}

fn lead_url(lead_id: i32) -> String {
    format!(
        "https://granite-manager.com/employee/deals/edit/{}/project",
        lead_id
    )
}

/// Из /start <email> вытаскиваем email (поддерживает /start@YourBot)
fn parse_start_email(text: &str) -> Option<String> {
    let mut it = text.trim().split_whitespace();
    let cmd = it.next()?;
    if !(cmd == "/start" || cmd.starts_with("/start@")) {
        return None;
    }
    let email = it.next()?; // ожидание: /start user@example.com
    Some(email.to_string())
}

fn gen_code() -> String {
    let n: u32 = rand::rng().random_range(0..=999_999);
    format!("{:06}", n)
}

fn new_resend_keyboard(email: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![InlineKeyboardButton::callback(
        "Send new code",
        format!("resend:{email}"),
    )]])
}

async fn handle_start_command(msg: Message, ctx: &AppState) -> Option<(StatusCode, String)> {
    let bot = &ctx.bot;
    let chat_id = msg.chat.id; // ChatId
    let chat_i64 = chat_id.0;

    if let Some(text) = msg.text() {
        // 1) /start <email> — регистрируемся, шлём код
        if let Some(email) = parse_start_email(text) {
            // генерим код
            let code = gen_code();

            // сохраняем состояние (3 попытки)
            ctx.verifications.insert(
                chat_i64,
                VerificationState {
                    email: email.clone(),
                    code: code.clone(),
                    attempts_left: 3,
                },
            );

            // отправляем письмо (заглушка/реальная интеграция)
            send_message(
                &[&email],
                &format!("Graninte Manager Code"),
                &format!("Your code is: {code}"),
            )
            .await.unwrap();

            // пишем пользователю
            let result = bot
                .send_message(
                    chat_id,
                    format!(
                        "You are now registering for {}, please enter the code sent to your email",
                        email
                    ),
                )
                .await;
            if let Err(e) = result {
                return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }

            return Some((StatusCode::OK, "ok".to_string()));
        }

        // 2) Если есть незавершённая верификация — пробуем интерпретировать текст как код
        if let Some(entry) = ctx.verifications.get(&chat_i64) {
            let user_code = text.trim();
            if user_code.trim() == entry.code.trim() {
                // успех
                let email = entry.email.clone();
                // можно здесь записать в БД связь chat_id <-> email
                // ctx.verifications.remove(&chat_i64);
                set_telegram_id(&ctx.pool, &email, &chat_i64.to_string()).await.unwrap();
                let result = bot
                    .send_message(chat_id, "Accepted, you are now registered")
                    .await;
                if let Err(e) = result {
                    return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                }

            } else {
                if entry.attempts_left > 1 {
                    let new_attempts = entry.attempts_left - 1;
                    ctx.verifications.insert(
                        chat_i64,
                        VerificationState {
                            email: entry.email.clone(),
                            code: entry.code.clone(),
                            attempts_left: new_attempts,
                        },
                    );
                    let result = bot
                        .send_message(
                            chat_id,
                            format!("Incorrect code. {} attempt(s) left.", new_attempts),
                        )
                        .await;
                    if let Err(e) = result {
                        return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                    }
                } else {
                    // попытки закончились — предлагаем отправить новый код
                    let email = entry.email.clone();
                    ctx.verifications.insert(
                        chat_i64,
                        VerificationState {
                            email: email.clone(),
                            code: entry.code.clone(),
                            attempts_left: 0,
                        },
                    );
                    let result = bot
                        .send_message(chat_id, "No attempts left.")
                        .reply_markup(new_resend_keyboard(&email))
                        .await;
                    if let Err(e) = result {
                        return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
                    }
                }
            }
        }
    }
    None
}

async fn handle_button(cb: CallbackQuery, ctx: &AppState) -> Option<(StatusCode, String)> {
    println!("handle_button: {:?}", cb);
    let bot = ctx.bot.clone();
    if let Some(data) = cb.data {
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
            ctx.verifications.insert(
                chat_i64,
                VerificationState {
                    email: email.to_string(),
                    code: code.clone(),
                    attempts_left: 3,
                },
            );

            // ACK
            let _ = bot.answer_callback_query(cb.id.clone()).await;

            // отправка письма
            send_message(
                &[&email],
                &format!("Your code is: {code}"),
                &format!("Your code is: {code}"),
            )
            .await.unwrap();

            // уведомляем
            let result = bot
                .send_message(
                    chat_id,
                    format!("A new code was sent to {}. Please enter it here.", email),
                )
                .await;
            if let Err(e) = result {
                return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }

            // можно также отредактировать исходное сообщение (если есть)
            if let Some(msg) = cb.message {
                let _ = bot
                    .edit_message_text(
                        msg.chat().id,
                        msg.id(),
                        "New code sent. Please enter it below.",
                    )
                    .await;
            }

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
                    format!("You were assigned a lead. Click here: \n#{}", lead_link),
                )
                .await;
            if let Err(e) = result {
                return Some((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }

            if let Some(msg) = cb.message {
                let _ = bot
                    .edit_message_text(
                        msg.chat().id,
                        msg.id(),
                        format!("Lead #{} assigned to {}", lead_id, user_chat_id),
                    )
                    .await;
            }
        }
    }
    None
}

pub async fn webhook_handler(
    State(ctx): State<AppState>,
    headers: HeaderMap,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    println!("STARTING WEBHOOK HANDLER");
    // Проверка секрета
    let secret = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::FORBIDDEN, "forbidden".to_string()))?;

    if secret != ctx.webhook_secret {
        return Err((StatusCode::FORBIDDEN, "forbidden".to_string()));
    }

    let result = match update.kind {
        UpdateKind::Message(msg) => handle_start_command(msg, &ctx).await,
        UpdateKind::CallbackQuery(cb) => handle_button(cb, &ctx).await,
        _ => None,
    };
    if let Some(response) = result {
        return Ok(response);
    }

    Ok((StatusCode::OK, "ok".to_string()))
}
