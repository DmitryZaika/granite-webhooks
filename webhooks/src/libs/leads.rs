use crate::axum_helpers::guards::Telegram;
use crate::cloudtalk::api::sync_customer_to_cloud_talk;
use crate::crud::leads::{
    Deal, ExistingCustomer, create_deal_from_lead, find_existing_customer,
    get_default_list_id_from_company_id, get_existing_deal, reset_deal_activity_deadlines,
    update_deal_list_id,
};
use crate::crud::users::{UserTgInfo, get_user_tg_info};
use crate::libs::alerts::alert_lead_not_notified;
use crate::libs::constants::{CREATED_RESPONSE, ERR_DB, internal_error};
use crate::libs::types::BasicResponse;
use crate::schemas::add_customer::LeadPayload;
use crate::telegram::send::{
    persist_lead_message, send_plain_message_to_chat, send_telegram_duplicate_notification,
    send_telegram_manager_assign,
};
use crate::telegram::utils::lead_url;
use common::amazon::email::send_message;
use common::crud::scheduled_emails::reschedule_templates_for_deal_list;
use lambda_http::tracing;
use reqwest::Client;
use sqlx::MySqlPool;

const REGISTER_SUBJECT: &str = "Granite Manager";
const REGISTER_MESSAGE: &str = "Connect Telegram in the CRM to receive lead notifications: https://granite-manager.com/link-telegram-both";

/// Tells the sales rep and the managers about a repeated lead; emails an
/// alert when neither could be reached in Telegram.
#[allow(clippy::too_many_arguments)]
async fn notify_repeat_lead<T, V: LeadPayload>(
    pool: &MySqlPool,
    company_id: i32,
    customer_id: i32,
    name: &str,
    deal_id: u64,
    user_id: i32,
    user_info: &UserTgInfo,
    form: &V,
    bot: &T,
) where
    T: Telegram + Send + Sync + 'static + Clone,
{
    let rep_notified = if let Some(chat_id) = user_info.telegram_id {
        let message = format!(
            "You received a REPEATED lead {name}, click here: {}",
            lead_url(deal_id)
        );
        match send_plain_message_to_chat(chat_id, &message, bot).await {
            Ok(message) => {
                persist_lead_message(pool, customer_id, company_id, &message).await;
                true
            }
            Err(request_error) => {
                tracing::error!(
                    ?request_error,
                    lead_id = customer_id,
                    "Employee notify failed"
                );
                false
            }
        }
    } else {
        // The rep hasn't connected Telegram: ask them to, and still tell the managers below.
        if let Err(e) = send_message(&[&user_info.email], REGISTER_SUBJECT, REGISTER_MESSAGE).await
        {
            tracing::error!(?e, company_id, "Failed to send message to email");
        }
        false
    };
    let managers_missed = send_telegram_duplicate_notification(
        pool,
        company_id,
        customer_id,
        name,
        user_id,
        form.to_string(),
        bot,
    )
    .await;
    if managers_missed && !rep_notified {
        let rep_problem = if user_info.telegram_id.is_some() {
            "the Telegram message to its sales rep failed"
        } else {
            "its sales rep has not connected Telegram"
        };
        let problem = format!(
            "Repeated lead {name} reached nobody in Telegram: {rep_problem} ({}), and no manager could be notified.",
            user_info.email
        );
        alert_lead_not_notified(company_id, &problem, form).await;
    }
}

async fn handle_repeat_lead<T, V: LeadPayload>(
    existing: &ExistingCustomer,
    deal: Deal,
    pool: &MySqlPool,
    company_id: i32,
    form: &V,
    bot: &T,
    new_deal: bool,
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
    let default_list_id = match get_default_list_id_from_company_id(pool, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, company_id = company_id, "Failed to get default list");
            return internal_error(ERR_DB);
        }
    };
    if let Err(e) = update_deal_list_id(pool, deal.id, default_list_id).await {
        tracing::error!(
            ?e,
            deal_id = deal.id,
            list_id = default_list_id,
            "Failed to move repeated lead to Not Contacted Yet"
        );
        return internal_error(ERR_DB);
    }
    if let Err(e) = reset_deal_activity_deadlines(pool, deal.id).await {
        tracing::error!(
            ?e,
            deal_id = deal.id,
            "Failed to reset activity due dates for repeated lead"
        );
        return internal_error(ERR_DB);
    }
    let customer_id = u64::try_from(existing.id).unwrap();
    let name = existing.name.as_deref().unwrap_or("Unknown");
    let Some(user_id) = deal.user_id else {
        // The deal has no sales rep (yet): ask the managers to assign one.
        let message = format!(
            "You received a REPEATED lead {name} with no sales rep, click here: {}\n{form}",
            lead_url(deal.id)
        );
        if let Err(e) =
            send_telegram_manager_assign(pool, company_id, message, customer_id, true, bot).await
        {
            tracing::error!(?e, company_id, "Failed to send message to Telegram");
            let problem = format!(
                "Repeated lead {name} has a deal without a sales rep, and the managers could not be asked in Telegram to assign one ({}).",
                e.1
            );
            alert_lead_not_notified(company_id, &problem, form).await;
        }
        return CREATED_RESPONSE;
    };
    if let Err(e) = reschedule_templates_for_deal_list(
        pool,
        default_list_id,
        company_id,
        deal.id,
        existing.id,
        user_id,
        new_deal,
    )
    .await
    {
        tracing::error!(
            ?e,
            deal_id = deal.id,
            list_id = default_list_id,
            "Failed to reschedule list drip emails for repeated lead"
        );
    }
    let user_info = match get_user_tg_info(pool, user_id).await {
        Ok(Some(info)) => info,
        Ok(None) => {
            let message = format!(
                "You received a REPEATED lead {name}, click here: {}",
                lead_url(deal.id)
            );
            if let Err(e) =
                send_telegram_manager_assign(pool, company_id, message, customer_id, false, bot)
                    .await
            {
                tracing::error!(?e, company_id, "Failed to send message to Telegram");
                let problem = format!(
                    "Repeated lead {name}: its sales rep (user #{user_id}) was not found, and the managers could not be notified in Telegram ({}).",
                    e.1
                );
                alert_lead_not_notified(company_id, &problem, form).await;
            }
            return CREATED_RESPONSE;
        }
        Err(e) => {
            tracing::error!(?e, user_id, "Failed to get user info");
            return internal_error(ERR_DB);
        }
    };

    notify_repeat_lead(
        pool,
        company_id,
        existing.id,
        name,
        deal.id,
        user_id,
        &user_info,
        form,
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
        let default_list_id = match get_default_list_id_from_company_id(pool, company_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(?e, company_id = company_id, "Failed to get default list");
                return Err(internal_error(ERR_DB));
            }
        };
        match create_deal_from_lead(pool, existing.id, rep.into(), default_list_id, 0).await {
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
    match send_telegram_manager_assign(pool, company_id, message, clean_id, false, bot).await {
        Ok(()) => Ok(None),
        Err(e) => {
            tracing::error!(
                ?e,
                company_id = company_id,
                "Failed to send message to Telegram"
            );
            let problem = format!(
                "Repeated lead from a customer with no sales rep could not be sent to the managers in Telegram ({}).",
                e.1
            );
            alert_lead_not_notified(company_id, &problem, form).await;
            Ok(None)
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
    let result = match form.insert(pool, company_id).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(?e, "Error creating lead from New Lead Form");
            return internal_error("Error creating lead from New Lead Form");
        }
    };
    let tg_result = send_telegram_manager_assign(
        pool,
        company_id,
        &form.to_string(),
        result.last_insert_id(),
        true,
        bot,
    )
    .await;
    if let Err(e) = tg_result {
        tracing::error!(
            ?e,
            company_id = company_id,
            "Error sending message to Telegram"
        );
        let problem = format!(
            "New lead could not be sent to the managers in Telegram, so nobody was asked to assign it ({}).",
            e.1
        );
        alert_lead_not_notified(company_id, &problem, form).await;
    }
    let customer_id = i32::try_from(result.last_insert_id()).unwrap_or(0);
    if customer_id > 0 {
        let client = Client::new();
        let _ = sync_customer_to_cloud_talk(pool, &client, customer_id).await;
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
            return handle_repeat_lead(&existing, deal, pool, company_id, form, bot, false).await;
        }
        Ok(None) => {
            let deal =
                create_new_deal_existing_customer(pool, &existing, company_id, form, bot).await;
            match deal {
                Ok(Some(deal)) => {
                    return handle_repeat_lead(
                        &existing,
                        deal,
                        pool,
                        company_id,
                        form,
                        bot,
                        true,
                    )
                    .await;
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
