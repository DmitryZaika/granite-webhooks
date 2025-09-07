use std::fmt::Display;

use crate::crud::leads::{
    create_lead_from_facebook, create_lead_from_new_lead_form, create_lead_from_wordpress,
};
use crate::crud::users::get_sales_users;
use crate::libs::constants::{CREATED_RESPONSE, ERR_DB, OK_RESPONSE, internal_error};
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::{FaceBookContactForm, NewLeadForm, WordpressContactForm};
use crate::telegram::send::send_lead_manager_message;
use axum::extract::Path;
use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn documenso() -> BasicResponse {
    OK_RESPONSE
}

async fn handle_telegram_send<T: Display>(
    pool: &MySqlPool,
    company_id: i32,
    data: T,
    customer_id: u64,
) -> Result<(), BasicResponse> {
    let all_users = match get_sales_users(pool, company_id).await {
        Ok(users) => users,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Error fetching users");
            return Err(internal_error(ERR_DB));
        }
    };
    let candidates: Vec<(String, i32, i64)> = all_users
        .iter()
        .filter(|item| item.position_id == Some(1))
        .map(|user| {
            (
                user.name.clone().unwrap_or_else(|| "Unknown".to_string()),
                user.id,
                user.mtd_lead_count,
            )
        })
        .collect();
    if let Some(telegram_id) = all_users
        .iter()
        .find(|u| u.position_id == Some(2))
        .and_then(|u| u.telegram_id)
    {
        let send_message =
            send_lead_manager_message(&data.to_string(), customer_id, telegram_id, &candidates)
                .await;
        if send_message.is_err() {
            tracing::error!(
                ?send_message,
                telegram_id = telegram_id,
                "Error sending message to lead manager"
            );
        }
    }
    Ok(())
}

pub async fn wordpress_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> BasicResponse {
    let result = match create_lead_from_wordpress(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from WordPress");
            return internal_error("Error creating lead from WordPress");
        }
    };
    let tg_result = handle_telegram_send(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
    )
    .await;

    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }

    CREATED_RESPONSE
}

pub async fn facebook_contact_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
) -> BasicResponse {
    let result = match create_lead_from_facebook(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from Facebook");
            return internal_error("Error creating lead from Facebook");
        }
    };

    let tg_result = handle_telegram_send(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
    )
    .await;

    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }

    CREATED_RESPONSE
}

pub async fn new_lead_form(
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<NewLeadForm>,
) -> BasicResponse {
    let result = match create_lead_from_new_lead_form(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from New Lead Form");
            return internal_error("Error creating lead from New Lead Form");
        }
    };

    let tg_result = handle_telegram_send(
        &pool,
        company_id,
        &contact_form.to_string(),
        result.last_insert_id(),
    )
    .await;

    if tg_result.is_err() {
        tracing::error!(
            ?tg_result,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        return internal_error("Error sending message to Telegram");
    }

    CREATED_RESPONSE
}
