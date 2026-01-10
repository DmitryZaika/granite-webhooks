use crate::amazon::email::send_message;
use crate::axum_helpers::guards::Telegram;
use crate::crud::leads::{
    Deal, ExistingCustomer, create_deal_from_lead, find_existing_customer, get_existing_deal,
};
use crate::crud::users::get_user_tg_info;
use crate::libs::constants::{
    CREATED_RESPONSE, ERR_DB, ERR_SEND_EMAIL, ERR_SEND_TELEGRAM, internal_error,
};
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::LeadPayload;
use crate::telegram::send::{
    send_plain_message_to_chat, send_telegram_duplicate_notification, send_telegram_manager_assign,
};
use crate::telegram::utils::lead_url;
use lambda_http::tracing;
use sqlx::MySqlPool;

const REGISTER_SUBJECT: &str = "Granite Manager";
const REGISTER_MESSAGE: &str = "Please register for Telegram to receive notifications about leads";

async fn handle_repeat_lead<T, V: LeadPayload>(
    existing: &ExistingCustomer,
    deal: Deal,
    pool: &MySqlPool,
    company_id: i32,
    form: &V,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    if let Err(e) = form.update(pool, company_id, existing.id).await {
        tracing::error!(
            ?e,
            company_id = company_id,
            existing_id = existing.id,
            "Failed to update lead"
        );
    }
    let customer_id = u64::try_from(existing.id).unwrap();
    let user_info = match get_user_tg_info(pool, deal.user_id.unwrap()).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            let name = existing.name.as_deref();
            let message = format!(
                "You received a REPEATED lead {}, click here: {}",
                name.unwrap_or("Unknown"),
                lead_url(deal.id)
            );
            match send_telegram_manager_assign(pool, company_id, message, customer_id, bot).await {
                Ok(()) => return CREATED_RESPONSE,
                Err(e) => {
                    tracing::error!(
                        ?e,
                        company_id = company_id,
                        "Failed to send message to Telegram"
                    );
                    return e;
                }
            }
        }
        Err(e) => {
            tracing::error!(
                ?e,
                user_id = deal.user_id.unwrap(),
                "Failed to get user info"
            );
            return internal_error(ERR_DB);
        }
    };

    let clean_tg_id = match user_info.telegram_id {
        Some(id) => id,
        None => match send_message(&[&user_info.email], REGISTER_SUBJECT, REGISTER_MESSAGE).await {
            Ok(()) => return CREATED_RESPONSE,
            Err(e) => {
                tracing::error!(
                    ?e,
                    company_id = company_id,
                    "Failed to send message to email"
                );
                return internal_error(ERR_SEND_EMAIL);
            }
        },
    };
    let repeted_lead_message = format!(
        "You received a REPEATED lead {}, click here: {}",
        existing.name.as_deref().unwrap_or("Unknown"),
        lead_url(deal.id)
    );
    let tg_result = send_plain_message_to_chat(clean_tg_id, &repeted_lead_message, bot).await;
    if let Err(request_error) = tg_result {
        tracing::error!(
            ?request_error,
            lead_id = existing.id,
            "Employee notify failed"
        );
        return internal_error(ERR_SEND_TELEGRAM);
    }
    let name = existing.name.as_deref().unwrap_or("Unknown");
    send_telegram_duplicate_notification(
        pool,
        company_id,
        name,
        deal.user_id.unwrap(),
        form.to_string(),
        bot,
    )
    .await;
    CREATED_RESPONSE
}

async fn create_new_deal_existing_customer<T, V: LeadPayload>(
    pool: &MySqlPool,
    existing: &ExistingCustomer,
    company_id: i32,
    form: &V,
    bot: &T,
) -> Result<Option<Deal>, BasicResponse>
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    if let Some(rep) = existing.sales_rep {
        match create_deal_from_lead(pool, existing.id, rep.into()).await {
            Ok(r) => {
                return Ok(Some(Deal {
                    id: r.last_insert_id(),
                    user_id: Some(rep),
                }));
            }
            Err(e) => {
                tracing::error!(?e, lead_id = existing.id, "Failed to create deal");
                return Err(internal_error(ERR_DB));
            }
        };
    }
    if let Err(e) = form.update(pool, company_id, existing.id).await {
        tracing::error!(
            ?e,
            company_id = company_id,
            existing_id = existing.id,
            "Failed to update lead"
        );
    }
    let clean_id = u64::try_from(existing.id).unwrap();
    let message = format!(
        "You received a REPEATED lead with no sales rep \n{form}",
        // form.to_string()
    );
    match send_telegram_manager_assign(pool, company_id, message, clean_id, bot).await {
        Ok(()) => Ok(None),
        Err(e) => {
            tracing::error!(
                ?e,
                company_id = company_id,
                "Failed to send message to Telegram"
            );
            Err(e)
        }
    }
}

async fn new_lead<T, V: LeadPayload>(
    pool: &MySqlPool,
    company_id: i32,
    form: &V,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    let result = match form.insert(&pool, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from New Lead Form");
            return internal_error("Error creating lead from New Lead Form");
        }
    };
    let tg_result = send_telegram_manager_assign(
        &pool,
        company_id,
        &form.to_string(),
        result.last_insert_id(),
        bot,
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

pub async fn process_lead<T, V: LeadPayload>(
    pool: &MySqlPool,
    company_id: i32,
    form: &V,
    bot: &T,
) -> BasicResponse
where
    T: Telegram + Send + Sync + 'static + Clone,
{
    let existing = match find_existing_customer(pool, form.email(), form.phone(), company_id).await
    {
        Ok(Some(v)) => v,
        Ok(None) => return new_lead(pool, company_id, form, bot).await,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Failed to check existing lead");
            return internal_error(ERR_DB);
        }
    };
    match get_existing_deal(pool, existing.id).await {
        Ok(Some(deal)) => {
            return handle_repeat_lead(&existing, deal, pool, company_id, form, bot).await;
        }
        Ok(None) => {
            let deal =
                create_new_deal_existing_customer(pool, &existing, company_id, form, bot).await;
            match deal {
                Ok(Some(deal)) => {
                    return handle_repeat_lead(&existing, deal, pool, company_id, form, bot).await;
                }
                Ok(None) => CREATED_RESPONSE,
                Err(e) => {
                    tracing::error!(?e, company_id = company_id, "Failed to create new deal");
                    e
                }
            }
        }
        Err(e) => {
            tracing::error!(?e, lead_id = existing.id, "Failed to check existing deal");
            internal_error(ERR_DB)
        }
    }
}
