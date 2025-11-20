use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

use crate::amazonses::schemas::SesEvent;
use crate::libs::constants::{BAD_REQUEST, OK_RESPONSE};
use crate::libs::types::BasicResponse;

pub async fn read_receipt_handler(
    State(pool): State<MySqlPool>,
    Json(info): Json<SesEvent>,
) -> BasicResponse {
    let message_id = info.detail.mail.message_id;
    let user_agent = info.detail.open.user_agent;
    let ip_address = info.detail.open.ip_address;
    let result = sqlx::query!(
        r#"
        INSERT INTO email_reads (message_id, user_agent, ip_address)
        VALUES (?, ?, ?)
        "#,
        message_id,
        user_agent,
        ip_address,
    )
    .execute(&pool)
    .await;

    if let Err(error) = result {
        tracing::error!(
            "Error inserting email: {} into the db: {}",
            message_id,
            error
        );
        return BAD_REQUEST;
    }
    OK_RESPONSE
}
