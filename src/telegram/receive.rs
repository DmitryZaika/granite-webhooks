use crate::crud::leads;
use crate::schemas::state::AppState;
use axum::http::StatusCode;
use axum::http::header::HeaderMap;
use axum::{extract::State, response::IntoResponse};
use teloxide::{
    prelude::*,
    types::{ChatId, Update, UpdateKind},
};
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

pub async fn webhook_sales_button(
    State(ctx): State<AppState>, // одно общее состояние
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

    // Разбираем update.kind
    if let UpdateKind::CallbackQuery(cb) = update.kind
        && let Some(data) = cb.data
    {
        if let Some((lead_id, user_chat_id)) = parse_assign(&data) {
            // ACK callback
            let _ = bot.answer_callback_query(cb.id.clone()).await;

            // Сообщение выбранному пользователю
            let lead_link = lead_url(lead_id);
            bot.send_message(
                ChatId(user_chat_id),
                format!("You were assigned a lead. Click here: \n#{}", lead_link),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

            // Опционально: обновить исходное сообщение
            if let Some(msg) = cb.message {
                let chat_id = msg.chat().id;
                let message_id = msg.id();
                let _ = bot
                    .edit_message_text(
                        chat_id,
                        message_id,
                        format!("Lead #{} assigned to {}", lead_id, user_chat_id),
                    )
                    .await;
            }
        }
    }

    Ok((StatusCode::OK, "ok"))
}
