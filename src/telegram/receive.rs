use crate::schemas::state::{AppState, VerificationState};
use axum::http::{StatusCode, header::HeaderMap};
use axum::{extract::State, response::IntoResponse};
use rand::Rng;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Update, UpdateKind};

// --- helpers ---

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

/// Заглушка отправки письма — здесь вставь интеграцию с почтой
async fn send_code_email(_pool: &sqlx::MySqlPool, email: &str, code: &str) {
    // TODO: интеграция с твоим email-провайдером
    // Пока просто логируем
    println!("VERIFICATION CODE for {email}: {code}");
}

fn new_resend_keyboard(email: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![InlineKeyboardButton::callback(
        "Send new code",
        format!("resend:{email}"),
    )]])
}

// --- handler ---

pub async fn webhook_sales_button(
    State(ctx): State<AppState>,
    headers: HeaderMap,
    axum::extract::Json(update): axum::extract::Json<Update>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Проверка секрета
    let secret = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::FORBIDDEN, "forbidden".to_string()))?;

    if secret != ctx.webhook_secret {
        return Err((StatusCode::FORBIDDEN, "forbidden".to_string()));
    }

    let bot = &ctx.bot;

    match update.kind {
        // ====== Сообщения: /start и ввод кода ======
        UpdateKind::Message(msg) => {
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
                    send_code_email(&ctx.pool, &email, &code).await;

                    // пишем пользователю
                    bot.send_message(
                        chat_id,
                        format!(
                            "You are now registering for {}, please enter the code sent to your email",
                            email
                        ),
                    )
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

                    return Ok((StatusCode::OK, "ok"));
                }

                // 2) Если есть незавершённая верификация — пробуем интерпретировать текст как код
                if let Some(mut entry) = ctx.verifications.get_mut(&chat_i64) {
                    let user_code = text.trim();
                    if user_code == entry.code {
                        // успех
                        let email = entry.email.clone();
                        // можно здесь записать в БД связь chat_id <-> email
                        ctx.verifications.remove(&chat_i64);

                        bot.send_message(chat_id, "Accepted, you are now registered")
                            .await
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

                        println!("User {chat_i64} verified as {email}");
                    } else {
                        if entry.attempts_left > 1 {
                            entry.attempts_left -= 1;
                            let left = entry.attempts_left;
                            bot.send_message(
                                chat_id,
                                format!("Incorrect code. {} attempt(s) left.", left),
                            )
                            .await
                            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                        } else {
                            // попытки закончились — предлагаем отправить новый код
                            let email = entry.email.clone();
                            entry.attempts_left = 0;
                            bot.send_message(chat_id, "No attempts left.")
                                .reply_markup(new_resend_keyboard(&email))
                                .await
                                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                        }
                    }
                }
            }
        }

        // ====== CallbackQuery: assign:* и resend:* ======
        UpdateKind::CallbackQuery(cb) => {
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
                    send_code_email(&ctx.pool, email, &code).await;

                    // уведомляем
                    bot.send_message(
                        chat_id,
                        format!("A new code was sent to {}. Please enter it here.", email),
                    )
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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

                    return Ok((StatusCode::OK, "ok"));
                }

                // B) Твоя существующая логика assign:lead_id:user_id
                if let Some((lead_id, user_chat_id)) = parse_assign(&data) {
                    // ACK
                    let _ = bot.answer_callback_query(cb.id.clone()).await;

                    let lead_link = lead_url(lead_id);
                    bot.send_message(
                        ChatId(user_chat_id),
                        format!("You were assigned a lead. Click here: \n#{}", lead_link),
                    )
                    .await
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
        }

        _ => {}
    }

    Ok((StatusCode::OK, "ok"))
}
