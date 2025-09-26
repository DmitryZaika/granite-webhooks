use crate::amazon::email::send_message;
use crate::crud::leads::{
    Deal, ExistingCustomer, LeadForm, create_deal_from_lead, find_existing_customer,
    get_existing_deal,
};
use crate::crud::users::get_user_tg_info;
use crate::libs::constants::{
    CREATED_RESPONSE, ERR_DB, ERR_SEND_EMAIL, ERR_SEND_TELEGRAM, internal_error,
};
use crate::libs::types::BasicResponse;
use crate::telegram::send::{send_plain_message_to_chat, send_telegram_manager_assign};
use crate::telegram::utils::lead_url;
use lambda_http::tracing;
use sqlx::MySqlPool;

async fn handle_repeat_lead(
    existing: &ExistingCustomer,
    deal: Deal,
    pool: &MySqlPool,
    company_id: i32,
    form: &LeadForm,
) -> BasicResponse {
    let name = existing.name.as_deref();
    let message = format!(
        "You received a repeated lead {}, click here: {}",
        name.unwrap_or("Unknown"),
        lead_url(deal.id)
    );
    form.update_lead(pool, company_id, existing.id).await;
    let customer_id = u64::try_from(existing.id).unwrap();
    let user_info = match get_user_tg_info(pool, deal.user_id.unwrap()).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            match send_telegram_manager_assign(pool, company_id, message, customer_id).await {
                Ok(_) => return CREATED_RESPONSE,
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
    let subject = format!("Granite Manager");
    let message = format!("Please register for Telegram to receive notifications about leads");
    let clean_tg_id = match user_info.telegram_id {
        Some(id) => id,
        None => match send_message(&[&user_info.email], &subject, &message).await {
            Ok(_) => return CREATED_RESPONSE,
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
        "You received a repeated lead {}, click here: {}",
        name.unwrap_or("Unknown"),
        lead_url(deal.id)
    );
    let tg_result = send_plain_message_to_chat(clean_tg_id, &repeted_lead_message).await;
    if let Err(request_error) = tg_result {
        tracing::error!(
            ?request_error,
            lead_id = existing.id,
            "Employee notify failed"
        );
        return internal_error(ERR_SEND_TELEGRAM);
    }
    CREATED_RESPONSE
}

async fn create_new_deal_existing_customer(
    pool: &MySqlPool,
    existing: &ExistingCustomer,
    company_id: i32,
    form: &LeadForm,
) -> Result<Option<Deal>, BasicResponse> {
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
    form.update_lead(pool, company_id, existing.id).await;
    let clean_id = u64::try_from(existing.id).unwrap();
    let name = existing.name.as_deref().unwrap_or("Unknown");
    let message = format!(
        "You received a REPEATED lead for {}, click here: {}",
        name,
        lead_url(clean_id)
    );
    match send_telegram_manager_assign(pool, company_id, message, clean_id).await {
        Ok(_) => return Ok(None),
        Err(e) => {
            tracing::error!(
                ?e,
                company_id = company_id,
                "Failed to send message to Telegram"
            );
            return Err(e);
        }
    }
}

pub async fn existing_lead_check(
    pool: &MySqlPool,
    email: &Option<&str>,
    phone: &Option<&str>,
    company_id: i32,
    form: &LeadForm,
) -> Option<BasicResponse> {
    let existing = match find_existing_customer(pool, email, phone, company_id).await {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Failed to check existing lead");
            return Some(internal_error(ERR_DB));
        }
    };
    match get_existing_deal(pool, existing.id).await {
        Ok(Some(deal)) => {
            return Some(handle_repeat_lead(&existing, deal, pool, company_id, form).await);
        }
        Ok(None) => {
            let deal = create_new_deal_existing_customer(pool, &existing, company_id, form).await;
            match deal {
                Ok(Some(deal)) => {
                    return Some(handle_repeat_lead(&existing, deal, pool, company_id, form).await);
                }
                Ok(None) => return Some(CREATED_RESPONSE),
                Err(e) => {
                    tracing::error!(?e, company_id = company_id, "Failed to create new deal");
                    return Some(e);
                }
            }
        }
        Err(e) => {
            tracing::error!(?e, lead_id = existing.id, "Failed to check existing deal");
            Some(internal_error(ERR_DB))
        }
    }
}
