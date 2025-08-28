use axum::http::StatusCode;
use axum::{
    extract::{Json, State},
    response::IntoResponse,
};
use teloxide::{
    prelude::*,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Update},
};

/*
async fn webhook(
    // State(state): State<AppState>,
    headers: HeaderMap,
    Json(update): Json<Update>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // Проверка секрета, если задан
    if let Some(expected) = &state.webhook_secret {
        if let Some(got) = headers.get("x-telegram-bot-api-secret-token") {
            if got != expected {
                return Err((StatusCode::FORBIDDEN, "forbidden".into()));
            }
        } else {
            return Err((StatusCode::FORBIDDEN, "forbidden".into()));
        }
    }

    if let Some(cb) = update.callback_query() {
        if let Some(data) = &cb.data {
            if let Some((lead_id, user_chat_id)) = parse_assign(data) {
                // Быстрый ACK колбэка
                let _ = state.bot.answer_callback_query(cb.id.clone()).await;

                // Сообщение выбранному пользователю
                state
                    .bot
                    .send_message(
                        ChatId(user_chat_id),
                        format!("Вам назначен лид #{}", lead_id),
                    )
                    .await
                    .map_err(internal)?;

                // (опционально) уведомить инициатора/отредактировать исходное сообщение
                if let Some(msg) = &cb.message {
                    let _ = state
                        .bot
                        .edit_message_text(
                            msg.chat.id,
                            msg.id,
                            format!("Назначено пользователю {}", user_chat_id),
                        )
                        .await;
                }
            }
        }
    }

    Ok((StatusCode::OK, "ok"))
}
 */
