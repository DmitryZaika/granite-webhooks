use crate::schemas::{EventBridgeEvent, OutgoingMessage};
use common::amazon::email::send_message;
use common::crud::notifications::{
    get_due_activity_deadline_reminders, mark_deadline_reminder_telegram_sent,
};
use common::crud::scheduled_emails::mark_scheduled_email_as_sent;
use common::crud::template::fetch_template_variable_data;
use common::crud::{scheduled_emails::get_ready_scheduled_emails, setup::create_db_pool};
use common::utils::template::replace_template_variables;
use lambda_runtime::{tracing, Error, LambdaEvent};
use sqlx::MySqlPool;
use teloxide::prelude::*;

async fn send_due_activity_deadline_reminders(pool: &MySqlPool) -> Result<usize, Error> {
    let reminders = get_due_activity_deadline_reminders(pool).await?;
    let bot = teloxide::Bot::from_env();
    let mut sent_count = 0usize;

    for reminder in reminders {
        let Some(telegram_id) = reminder.telegram_id else {
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

/// There are some code example in the following URLs:
/// - https://github.com/awslabs/aws-lambda-rust-runtime/tree/main/examples
/// - https://github.com/aws-samples/serverless-rust-demo/
pub(crate) async fn function_handler(
    _pool: &MySqlPool,
    event: LambdaEvent<EventBridgeEvent>,
) -> Result<OutgoingMessage, Error> {
    // This will now print the full JSON structure to your CloudWatch logs
    tracing::info!("Received event: {:?}", event.payload);

    let pool = create_db_pool().await?;
    let ready_emails = get_ready_scheduled_emails(&pool).await?;
    for email in &ready_emails {
        let data = fetch_template_variable_data(
            &pool,
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
        send_message(&[&cleaned_email], &email.template_subject, &result).await?;
        mark_scheduled_email_as_sent(&pool, email.id).await?;
    }
    let reminder_count = send_due_activity_deadline_reminders(&pool).await?;
    let message = format!(
        "Successfully processed {} emails and {} activity deadline reminders",
        ready_emails.len(),
        reminder_count
    );
    let resp = OutgoingMessage::new(event.context.request_id, message.clone());
    tracing::info!("{}", message);

    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lambda_runtime::{Context, LambdaEvent};

    #[sqlx::test]
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
