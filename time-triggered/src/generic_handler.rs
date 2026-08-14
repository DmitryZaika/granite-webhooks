use crate::schemas::{EventBridgeEvent, OutgoingMessage};
use common::amazon::email::{assigned_sender_from, send_message_from};
use common::crud::notifications::{
    get_due_activity_deadline_reminders, mark_deadline_reminder_telegram_sent,
};
use common::crud::scheduled_emails::{
    cancel_pending_emails_for_non_leads, cancel_pending_emails_left_list,
    get_ready_scheduled_emails, mark_scheduled_email_as_sent,
};
use common::crud::template::fetch_template_variable_data;
use common::utils::template::replace_template_variables;
use lambda_runtime::{tracing, Error, LambdaEvent};
use reqwest::Client;
use sqlx::MySqlPool;
use teloxide::prelude::*;

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
        let text = common::telegram::crm::format_activity_notification(
            "activity_deadline_reminder",
            reminder.customer_name.as_deref(),
            None,
            &reminder.message,
            i32::try_from(reminder.deal_id).unwrap_or(i32::MAX),
        );
        match bot.send_message(ChatId(telegram_id), text).await {
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

    let url = format!("{}/{}", app_url.trim_end_matches('/'), path.trim_start_matches('/'));
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
        let data = fetch_template_variable_data(
            pool,
            email.user_id,
            Some(email.deal_id),
            Some(email.customer_id),
            email.company_id,
        )
        .await
        .unwrap();
        let result = replace_template_variables(&email.template_body, &data);
        let cleaned_email = match &email.email {
            Some(email) => email,
            None => {
                tracing::warn!(
                    "Skipping customer_id: {}, no email address",
                    email.customer_id
                );
                continue;
            }
        };
        let from = assigned_sender_from(
            data.company.as_ref().and_then(|company| company.domain.as_deref()),
            data.user.email.as_deref(),
            data.user.email_name.as_deref(),
        );
        send_message_from(&[&cleaned_email], &email.template_subject, &result, &from).await?;
        mark_scheduled_email_as_sent(pool, email.id).await?;
    }
    let reminder_count = send_due_activity_deadline_reminders(pool).await?;
    let estimate_reminder_count = process_estimate_appointment_reminders().await?;
    let maintenance_reminder_count = process_maintenance_due_reminders().await?;
    let message = format!(
        "Successfully processed {} emails, {} activity deadline reminders, {} estimate appointment reminders, and {} maintenance due reminders",
        ready_emails.len(),
        reminder_count,
        estimate_reminder_count,
        maintenance_reminder_count
    );
    let resp = OutgoingMessage::new(event.context.request_id, message.clone());
    tracing::info!("{}", message);

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

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
