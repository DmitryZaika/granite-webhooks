use crate::axum_helpers::guards::MarketingUser;
use crate::crud::leads::LeadForm;
use crate::crud::leads::{
    create_lead_from_facebook, create_lead_from_new_lead_form, create_lead_from_wordpress,
};
use crate::libs::constants::{CREATED_RESPONSE, OK_RESPONSE, internal_error};
use crate::libs::leads::existing_lead_check;
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::{FaceBookContactForm, NewLeadForm, WordpressContactForm};
use crate::telegram::send::send_telegram_manager_assign;
use axum::extract::Path;
use axum::extract::{Json, State};
use lambda_http::tracing;
use sqlx::MySqlPool;

pub async fn documenso() -> BasicResponse {
    OK_RESPONSE
}

pub async fn wordpress_contact_form(
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<WordpressContactForm>,
) -> BasicResponse {
    if let Some(response) = existing_lead_check(
        &pool,
        &contact_form.email.as_deref(),
        &contact_form.phone.as_deref(),
        company_id,
        &LeadForm::WordpressContactForm(contact_form.clone()),
    )
    .await
    {
        return response;
    }
    let result = match create_lead_from_wordpress(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from WordPress");
            return internal_error("Error creating lead from WordPress");
        }
    };
    let tg_result = send_telegram_manager_assign(
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
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<FaceBookContactForm>,
    // _: MarketingUser,
) -> BasicResponse {
    if let Some(response) = existing_lead_check(
        &pool,
        &contact_form.email.as_deref(),
        &contact_form.phone.as_deref(),
        company_id,
        &LeadForm::FaceBookContactForm(contact_form.clone()),
    )
    .await
    {
        return response;
    }

    let result = match create_lead_from_facebook(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from Facebook");
            return internal_error("Error creating lead from Facebook");
        }
    };

    let tg_result = send_telegram_manager_assign(
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
    _: MarketingUser,
    Path(company_id): Path<i32>,
    State(pool): State<MySqlPool>,
    Json(contact_form): Json<NewLeadForm>,
    // _: MarketingUser,
) -> BasicResponse {
    let existing_result = existing_lead_check(
        &pool,
        &contact_form.email.as_deref(),
        &contact_form.phone.as_deref(),
        company_id,
        &LeadForm::NewLeadForm(contact_form.clone()),
    )
    .await;
    if let Some(response) = existing_result {
        return response;
    }

    let result = match create_lead_from_new_lead_form(&pool, &contact_form, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from New Lead Form");
            return internal_error("Error creating lead from New Lead Form");
        }
    };
    let tg_result = send_telegram_manager_assign(
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
