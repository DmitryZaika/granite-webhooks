use crate::schemas::{EventBridgeEvent, OutgoingMessage};
use common::amazon::email::{assigned_sender_from, send_message_from};
use common::crud::notifications::{
    get_due_activity_deadline_reminders, mark_deadline_reminder_telegram_sent,
};
use common::crud::outbound_email::{
    OutboundScheduledEmail, record_outbound_scheduled_email,
};
use common::crud::scheduled_emails::{
    cancel_pending_emails_for_non_leads, cancel_pending_emails_left_list,
    get_ready_scheduled_emails, mark_scheduled_email_as_sent,
    mark_scheduled_email_failed_with_reason, ScheduledEmail,
};
use common::crud::template::fetch_template_variable_data;
use common::utils::template::replace_template_variables;
use lambda_runtime::{tracing, Error, LambdaEvent};
use reqwest::Client;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

async fn send_due_activity_deadline_reminders(pool: &MySqlPool) -> Result<usize, Error> {
    let reminders = get_due_activity_deadline_reminders(pool).await?;
    if reminders.is_empty() {
        return Ok(0);
    }
    let token = std::env::var("TELOXIDE_NOTIFICATIONS_TOKEN")
        .or_else(|_| std::env::var("TELEGRAM_NOTIFICATIONS_BOT_TOKEN"))
        .map_err(|error| Error::from(error.to_string()))?;
    let bot = teloxide::Bot::new(token);
    let mut sent_count = 0usize;

    for reminder in reminders {
        if !reminder.telegram_activity_notifications {
            continue;
        }
        let Some(telegram_id) = reminder.notifications_telegram_id else {
            continue;
        };
        let message = common::telegram::crm::format_activity_notification(
            "activity_deadline_reminder",
            reminder.customer_name.as_deref(),
            None,
            &reminder.message,
            i32::try_from(reminder.deal_id).unwrap_or(i32::MAX),
        );
        let Ok(button_url) = message.button_url.parse::<reqwest::Url>() else {
            tracing::error!(
                notification_id = reminder.id,
                url = %message.button_url,
                "Invalid activity reminder telegram button url"
            );
            continue;
        };
        let keyboard = InlineKeyboardMarkup::new([[InlineKeyboardButton::url(
            message.button_label.to_string(),
            button_url,
        )]]);
        match bot
            .send_message(ChatId(telegram_id), message.text)
            .reply_markup(keyboard)
            .await
        {
            Ok(_) => {
                mark_deadline_reminder_telegram_sent(pool, reminder.id).await?;
                sent_count += 1;
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    notification_id = reminder.id,
                    user_id = reminder.user_id,
                    "Failed to send activity deadline reminder telegram notification"
                );
            }
        }
    }

    Ok(sent_count)
}

async fn post_app_process_route(path: &str, label: &str) -> Result<usize, Error> {
    let app_url = match std::env::var("APP_URL") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(?error, "APP_URL is not set; skipping {label}");
            return Ok(0);
        }
    };
    let lambda_key = match std::env::var("LAMBDA_KEY") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(?error, "LAMBDA_KEY is not set; skipping {label}");
            return Ok(0);
        }
    };

    let url = format!(
        "{}/{}",
        app_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let client = Client::new();
    let response = client
        .post(url)
        .header("Authorization", lambda_key)
        .send()
        .await
        .map_err(|error| Error::from(error.to_string()))?;

    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "Failed to process {label}"
        );
        return Ok(0);
    }

    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| Error::from(error.to_string()))?;
    let processed = body
        .get("processed")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    Ok(usize::try_from(processed).unwrap_or(0))
}

async fn process_estimate_appointment_reminders() -> Result<usize, Error> {
    post_app_process_route("api/estimate-reminders/process", "estimate reminders").await
}

async fn process_maintenance_due_reminders() -> Result<usize, Error> {
    post_app_process_route(
        "api/maintenance-reminders/process",
        "maintenance due reminders",
    )
    .await
}

async fn process_sms_followups() -> Result<usize, Error> {
    post_app_process_route("api/sms-followups/process", "sms follow-ups").await
}

async fn process_checklist_surveys() -> Result<usize, Error> {
    post_app_process_route(
        "api/survey-notifications/process",
        "checklist survey notifications",
    )
    .await
}

pub fn scheduled_email_recipient(email: Option<&str>) -> Option<&str> {
    email.map(str::trim).filter(|value| !value.is_empty())
}

async fn send_and_record_scheduled_email(
    pool: &MySqlPool,
    email: &ScheduledEmail,
) -> Result<(), Error> {
    let Some(cleaned_email) = scheduled_email_recipient(email.email.as_deref()) else {
        mark_scheduled_email_failed_with_reason(
            pool,
            email.id,
            "Customer has no email address",
        )
        .await?;
        tracing::warn!(
            customer_id = email.customer_id,
            scheduled_email_id = email.id,
            "Skipping automated email, no email address"
        );
        return Ok(());
    };
    let data = fetch_template_variable_data(
        pool,
        email.user_id,
        Some(email.deal_id),
        Some(email.customer_id),
        email.company_id,
    )
    .await
    .map_err(|error| Error::from(error.to_string()))?;
    let html_body = replace_template_variables(&email.template_body, &data);
    let from = assigned_sender_from(
        data.company
            .as_ref()
            .and_then(|company| company.domain.as_deref()),
        data.user.email.as_deref(),
        data.user.email_name.as_deref(),
    );
    let message_id =
        send_message_from(&[cleaned_email], &email.template_subject, &html_body, &from)
            .await
            .map_err(|error| Error::from(error.to_string()))?;
    if let Err(error) = record_outbound_scheduled_email(
        pool,
        &OutboundScheduledEmail {
            scheduled_email_id: email.id,
            user_id: email.user_id,
            customer_id: email.customer_id,
            company_id: email.company_id,
            deal_id: email.deal_id,
            subject: email.template_subject.clone(),
            html_body,
            sender_from: from,
            recipient_email: cleaned_email.to_string(),
            message_id,
        },
    )
    .await
    {
        tracing::error!(
            ?error,
            scheduled_email_id = email.id,
            "Failed to save automated email to the conversation"
        );
    }
    if let Err(error) = mark_scheduled_email_as_sent(pool, email.id).await {
        tracing::error!(
            ?error,
            scheduled_email_id = email.id,
            "Failed to mark automated email as sent after SES send"
        );
    }
    Ok(())
}

/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
/// - https://github.com/aws-samples/serverless-rust-demo/
pub(crate) async fn function_handler(
    pool: &MySqlPool,
    event: LambdaEvent<EventBridgeEvent>,
) -> Result<OutgoingMessage, Error> {
    // This will now print the full JSON structure to your CloudWatch logs
    tracing::info!("Received event: {:?}", event.payload);

    cancel_pending_emails_left_list(pool).await?;
    cancel_pending_emails_for_non_leads(pool).await?;
    let ready_emails = get_ready_scheduled_emails(pool).await?;
    for email in &ready_emails {
        if let Err(error) = send_and_record_scheduled_email(pool, email).await {
            tracing::error!(
                ?error,
                scheduled_email_id = email.id,
                customer_id = email.customer_id,
                "Failed to send automated email"
            );
            if let Err(mark_error) =
                mark_scheduled_email_failed_with_reason(pool, email.id, &error.to_string())
                    .await
            {
                tracing::error!(
                    ?mark_error,
                    scheduled_email_id = email.id,
                    "Failed to mark automated email as failed"
                );
            }
        }
    }
    let reminder_count = send_due_activity_deadline_reminders(pool).await?;
    let estimate_reminder_count = process_estimate_appointment_reminders().await?;
    let maintenance_reminder_count = process_maintenance_due_reminders().await?;
    let sms_followup_count = process_sms_followups().await?;
    let checklist_survey_count = process_checklist_surveys().await?;
    let message = format!(
        "Successfully processed {} emails, {} activity deadline reminders, {} estimate appointment reminders, {} maintenance due reminders, {} sms follow-ups, and {} checklist surveys",
        ready_emails.len(),
        reminder_count,
        estimate_reminder_count,
        maintenance_reminder_count,
        sms_followup_count,
        checklist_survey_count
    );
    let resp = OutgoingMessage::new(event.context.request_id, message.clone());
    tracing::info!("{}", message);

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

    #[test]
    fn scheduled_email_recipient_requires_a_non_empty_address() {
        assert_eq!(scheduled_email_recipient(None), None);
        assert_eq!(scheduled_email_recipient(Some("   ")), None);
        assert_eq!(
            scheduled_email_recipient(Some(" beattyheather@yahoo.com ")),
            Some("beattyheather@yahoo.com")
        );
        assert_eq!(
            scheduled_email_recipient(Some("dema.gdindy@gmail.com")),
            Some("dema.gdindy@gmail.com")
        );
    }

    #[test]
    fn send_and_record_saves_history_before_marking_sent() {
        let source = include_str!("generic_handler.rs");
        let fn_start = source
            .find("async fn send_and_record_scheduled_email")
            .expect("send function");
        let fn_end = source
            .find("pub(crate) async fn function_handler")
            .expect("handler");
        let body = &source[fn_start..fn_end];
        let record_at = body
            .find("record_outbound_scheduled_email")
            .expect("record call");
        let sent_at = body
            .find("mark_scheduled_email_as_sent")
            .expect("mark sent call");
        assert!(
            record_at < sent_at,
            "History row must be written before the drip is marked sent"
        );
    }

    #[test]
    fn function_handler_processes_checklist_surveys_after_sms_followups() {
        let source = include_str!("generic_handler.rs");
        assert!(source.contains("api/survey-notifications/process"));
        let sms_at = source
            .find("process_sms_followups")
            .expect("sms follow-ups call");
        let survey_at = source
            .find("process_checklist_surveys")
            .expect("checklist survey call");
        assert!(
            sms_at < survey_at,
            "Checklist surveys should run on the same scheduled tick as SMS follow-ups"
        );
        assert!(source.contains("checklist surveys"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn test_generic_handler(pool: MySqlPool) {
        // Mocking the data we saw in the logs
        let incoming = EventBridgeEvent {
            account: "123456789012".to_string(),
            detail: serde_json::json!({}),
            detail_type: "Scheduled Event".to_string(),
            id: "uuid-1234".to_string(),
            region: "us-east-2".to_string(),
            resources: vec!["arn:aws:scheduler...".to_string()],
            source: "aws.scheduler".to_string(),
            time: "2026-04-19T16:04:00Z".to_string(),
            version: "0".to_string(),
        };

        let event = LambdaEvent::new(incoming, Context::default());
        let response = function_handler(&pool, event).await.unwrap();

        // Adjusting expectation to match the actual fields
        assert!(response.msg.contains("Successfully processed "));
    }
}
