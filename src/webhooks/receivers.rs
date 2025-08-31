use std::fmt::Display;

use crate::crud::leads::{create_lead_from_facebook, create_lead_from_wordpress};
use crate::crud::users::get_sales_users;
use crate::schemas::add_customer::{FaceBookContactForm, WordpressContactForm};
use crate::schemas::documenso::WebhookEvent;
use crate::telegram::send::send_lead_manager_message;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::{
    extract::{Json, State},
    response::IntoResponse,
};
use sqlx::MySqlPool;

pub async fn documenso(payload: Json<WebhookEvent>) -> impl IntoResponse {
    println!("Received documenso webhook event: {payload:?}");
    StatusCode::OK
}

async fn handle_telegram_send<T: Display>(
    pool: &MySqlPool,
    company_id: i32,
    data: T,
    customer_id: u64,
) {
    let all_users = get_sales_users(pool, company_id).await.unwrap();
    let candidates: Vec<(String, i32, i64)> = all_users
        .iter()
        .map(|user| {
            (
                user.name.clone().unwrap_or("Unknown".to_string()),
                user.id,
                user.mtd_lead_count,
            )
        })
        .collect();
    let sales_manager = all_users
        .iter()
        .find(|item| item.position_id == Some(2) && item.telegram_id.is_some());
    if let Some(manager) = sales_manager {
        send_lead_manager_message(
            &data.to_string(),
            customer_id,
            manager.telegram_id.unwrap(),
            &candidates,
        )
        .await
        .unwrap();
    }
}

pub async fn wordpress_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let result = create_lead_from_wordpress(&pool, &contact_form, company_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    handle_telegram_send(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
    )
    .await;

    Ok((StatusCode::CREATED, "created").into_response())
}

pub async fn facebook_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let result = create_lead_from_facebook(&pool, &contact_form, company_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    handle_telegram_send(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
    )
    .await;

    Ok((StatusCode::CREATED, "created").into_response())
}
