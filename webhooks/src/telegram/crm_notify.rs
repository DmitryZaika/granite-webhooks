use crate::axum_helpers::guards::{RemixBackend, TelegramBot};
use crate::libs::constants::OK_RESPONSE;
use crate::libs::types::BasicResponse;
use crate::telegram::crm::{CrmTelegramNotify, send_crm_telegram_notification};
use axum::Json;
use axum::extract::State;
use lambda_http::tracing;
use serde::Deserialize;
use sqlx::MySqlPool;

#[derive(Debug, Deserialize)]
pub struct CrmNotifyRequest {
    pub user_id: i32,
    pub deal_id: i32,
    pub notification_type: String,
    pub message: String,
    pub actor_name: Option<String>,
    pub customer_name: Option<String>,
}

pub async fn crm_notify_handler(
    _: RemixBackend,
    State(pool): State<MySqlPool>,
    Json(body): Json<CrmNotifyRequest>,
) -> BasicResponse {
    let bot = TelegramBot::default();
    let payload = CrmTelegramNotify {
        user_id: body.user_id,
        deal_id: body.deal_id,
        notification_type: body.notification_type,
        message: body.message,
        actor_name: body.actor_name,
        customer_name: body.customer_name,
    };
    if let Err(error) = send_crm_telegram_notification(&pool, &bot, &payload).await {
        tracing::error!(?error, user_id = body.user_id, "CRM telegram notify failed");
    }
    OK_RESPONSE
}
