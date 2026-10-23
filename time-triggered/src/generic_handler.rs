use crate::posthog::{report_failure, reported};
use crate::schemas::{EventBridgeEvent, OutgoingMessage};
use common::amazon::email::{assigned_sender_from, send_message_from};
use common::crud::notifications::{
    get_due_activity_deadline_reminders, mark_deadline_reminder_telegram_sent,
    unmark_deadline_reminder_telegram_sent,
};
use common::crud::outbound_email::{OutboundScheduledEmail, record_outbound_scheduled_email};
use common::crud::scheduled_emails::{
    ScheduledEmail, cancel_pending_emails_for_non_leads, cancel_pending_emails_left_list,
    get_ready_scheduled_emails, mark_scheduled_email_as_sent,
    mark_scheduled_email_failed_with_reason,
};
use common::crud::template::fetch_template_variable_data;
use common::utils::email_send_window::automated_email_send_window_is_open_now;
use common::utils::template::replace_template_variables;
use lambda_runtime::{Error, LambdaEvent, tracing};
use reqwest::Client;
use sqlx::MySqlPool;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

fn activity_reminder_needs_telegram(
    telegram_activity_notifications: bool,
    notifications_telegram_id: Option<i64>,
) -> bool {
    telegram_activity_notifications && notifications_telegram_id.is_some()
}

async fn send_due_activity_deadline_reminders(pool: &MySqlPool) -> Result<usize, Error> {
    let reminders = get_due_activity_deadline_reminders(pool).await?;
    let sendable = reminders.iter().any(|reminder| {
        activity_reminder_needs_telegram(
            reminder.telegram_activity_notifications,
            reminder.notifications_telegram_id,
        )
    });
    if !sendable {
        return Ok(0);
    }
    let token = match std::env::var("TELOXIDE_NOTIFICATIONS_TOKEN")
        .or_else(|_| std::env::var("TELEGRAM_NOTIFICATIONS_BOT_TOKEN"))
    {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                ?error,
                "Telegram notifications token is not set; skipping activity deadline reminders"
            );
            return Ok(0);
        }
    };
    let bot = teloxide::Bot::new(token);
    let mut sent_count = 0usize;

    for reminder in reminders {
        if !activity_reminder_needs_telegram(
            reminder.telegram_activity_notifications,
            reminder.notifications_telegram_id,
        ) {
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
        let marked = mark_deadline_reminder_telegram_sent(pool, reminder.id).await?;
        if marked.rows_affected() < 1 {
            continue;
        }
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
                sent_count += 1;
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    notification_id = reminder.id,
                    user_id = reminder.user_id,
                    "Failed to send activity deadline reminder telegram notification"
                );
                unmark_deadline_reminder_telegram_sent(pool, reminder.id).await?;
            }
        }
    }

    Ok(sent_count)
}

/// Why a job of the API did not run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct JobError {
    message: String,
    /// The api answered 5xx: it reported that error to `PostHog` itself.
    api_reported: bool,
}

impl JobError {
    fn transport(error: &reqwest::Error) -> Self {
        Self {
            message: error.to_string(),
            api_reported: false,
        }
    }

    /// `Err` for any non-success answer; 5xx ones are already reported by the api.
    fn from_status(status: u16) -> Result<(), Self> {
        match status {
            200..=299 => Ok(()),
            _ => Err(Self {
                message: format!("the api answered HTTP {status}"),
                api_reported: (500..=599).contains(&status),
            }),
        }
    }

    pub(crate) const fn should_report(&self) -> bool {
        !self.api_reported
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

/// Reports a job's failure unless the api already did.
async fn report_job(job: &str, result: Result<usize, JobError>) -> Result<usize, JobError> {
    match result {
        Err(error) if !error.should_report() => Err(error),
        other => reported(job, other).await,
    }
}

/// The API answers within its 30 s limit; a hung connection must not use up
/// the whole run and starve the jobs after it.
const API_JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);

/// Runs a scheduled job of the API (`POST <API_URL>/v1/jobs/<job>`).
async fn post_api_job(job: &str, label: &str) -> Result<usize, JobError> {
    post_api_job_within(job, label, API_JOB_TIMEOUT).await
}

/// [`post_api_job`], giving up after `timeout`.
async fn post_api_job_within(
    job: &str,
    label: &str,
    timeout: std::time::Duration,
) -> Result<usize, JobError> {
    let api_url = match std::env::var("API_URL") {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(?error, "API_URL is not set; skipping {label}");
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
    let url = format!("{}/v1/jobs/{job}", api_url.trim_end_matches('/'));
    let response = Client::new()
        .post(url)
        .header("Authorization", lambda_key)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| JobError::transport(&error))?;
    if let Err(error) = JobError::from_status(response.status().as_u16()) {
        tracing::warn!(
            status = response.status().as_u16(),
            "Failed to process {label}"
        );
        // The tick goes on whatever the api answered. A 5xx the api reported
        // itself; any other answer (a wrong key, a missing route) only shows
        // here.
        if error.should_report() {
            report_failure(job, &error).await;
        }
        return Ok(0);
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| JobError::transport(&error))?;
    let processed = body
        .get("processed")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    Ok(usize::try_from(processed).unwrap_or(0))
}

/// The job's count, or 0 after reporting and logging: one failing job must
/// not stop the jobs after it.
async fn or_logged(job: &str, result: Result<usize, JobError>, label: &str) -> usize {
    report_job(job, result).await.unwrap_or_else(|error| {
        tracing::error!(?error, "Failed to process {label}; continuing");
        0
    })
}

async fn process_estimate_appointment_reminders() -> Result<usize, JobError> {
    post_api_job("appointment-reminders", "estimate reminders").await
}

async fn process_maintenance_due_reminders() -> Result<usize, JobError> {
    post_api_job("maintenance-reminders", "maintenance due reminders").await
}

async fn process_sms_followups() -> Result<usize, JobError> {
    post_api_job("sms-followups", "sms follow-ups").await
}

async fn process_call_points() -> Result<usize, JobError> {
    post_api_job("call-points", "call points").await
}

async fn process_signature_reminders() -> Result<usize, JobError> {
    post_api_job("signature-reminders", "e-signature reminders").await
}

/// The api answers within its 30 s; a call that hangs must not hold the tick.
async fn process_posthog_autofix() -> Result<usize, JobError> {
    post_api_job_within(
        "posthog-autofix",
        "PostHog auto-fix",
        std::time::Duration::from_secs(25),
    )
    .await
}

async fn process_checklist_surveys() -> Result<usize, JobError> {
    post_api_job("survey-notifications", "checklist survey notifications").await
}

async fn process_contact_sync() -> Result<usize, JobError> {
    post_api_job("contact-sync", "contact sync").await
}

/// Starts the supplier price file job without waiting for it: a big PDF takes
/// minutes, and the API Lambda keeps working after this request gives up.
/// Sent to the long-running deployment (`API_STREAM_URL`) when there is one.
async fn start_supplier_files_job() {
    let Some(api_url) = std::env::var("API_STREAM_URL")
        .ok()
        .or_else(|| std::env::var("API_URL").ok())
    else {
        tracing::warn!("API_URL is not set; skipping supplier files");
        return;
    };
    let Ok(lambda_key) = std::env::var("LAMBDA_KEY") else {
        tracing::warn!("LAMBDA_KEY is not set; skipping supplier files");
        return;
    };
    let url = format!("{}/v1/jobs/supplier-files", api_url.trim_end_matches('/'));
    let sent = Client::new()
        .post(url)
        .header("Authorization", lambda_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    match sent {
        Ok(response) if !response.status().is_success() => tracing::warn!(
            status = response.status().as_u16(),
            "Failed to start supplier files"
        ),
        Err(error) if !error.is_timeout() => {
            tracing::warn!(?error, "Failed to start supplier files");
        }
        _ => {}
    }
}

pub fn scheduled_email_recipient(email: Option<&str>) -> Option<&str> {
    email.map(str::trim).filter(|value| !value.is_empty())
}

async fn send_and_record_scheduled_email(
    pool: &MySqlPool,
    email: &ScheduledEmail,
) -> Result<(), Error> {
    let Some(cleaned_email) = scheduled_email_recipient(email.email.as_deref()) else {
        mark_scheduled_email_failed_with_reason(pool, email.id, "Customer has no email address")
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

    reported(
        "cancel-pending-emails",
        cancel_pending_emails_left_list(pool).await,
    )
    .await?;
    reported(
        "cancel-pending-emails",
        cancel_pending_emails_for_non_leads(pool).await,
    )
    .await?;
    let ready_emails = reported("scheduled-emails", get_ready_scheduled_emails(pool).await).await?;
    let processed_email_count = if automated_email_send_window_is_open_now() {
        let mut failed = 0usize;
        let mut first_error = None;
        for email in &ready_emails {
            if let Err(error) = send_and_record_scheduled_email(pool, email).await {
                tracing::error!(
                    ?error,
                    scheduled_email_id = email.id,
                    customer_id = email.customer_id,
                    "Failed to send automated email"
                );
                failed += 1;
                first_error.get_or_insert_with(|| error.to_string());
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
        if let Some(first) = first_error {
            report_failure(
                "scheduled-emails",
                &format!(
                    "{failed} of {} scheduled emails failed to send; first error: {first}",
                    ready_emails.len()
                ),
            )
            .await;
        }
        ready_emails.len()
    } else {
        0
    };
    let reminder_count = match send_due_activity_deadline_reminders(pool).await {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(
                ?error,
                "Failed to send activity deadline reminders; continuing scheduled work"
            );
            report_failure("activity-deadline-reminders", &error).await;
            0
        }
    };
    let estimate_reminder_count = or_logged(
        "appointment-reminders",
        process_estimate_appointment_reminders().await,
        "estimate reminders",
    )
    .await;
    let maintenance_reminder_count = or_logged(
        "maintenance-reminders",
        process_maintenance_due_reminders().await,
        "maintenance due reminders",
    )
    .await;
    let sms_followup_count = or_logged(
        "sms-followups",
        process_sms_followups().await,
        "sms follow-ups",
    )
    .await;
    let checklist_survey_count = or_logged(
        "survey-notifications",
        process_checklist_surveys().await,
        "checklist surveys",
    )
    .await;
    start_supplier_files_job().await;
    let call_points_count = match report_job("call-points", process_call_points().await).await {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(?error, "Failed to process call points; continuing");
            0
        }
    };
    let signature_reminder_count =
        match report_job("signature-reminders", process_signature_reminders().await).await {
            Ok(count) => count,
            Err(error) => {
                tracing::error!(
                    ?error,
                    "Failed to process e-signature reminders; continuing"
                );
                0
            }
        };
    let contact_sync_count = match report_job("contact-sync", process_contact_sync().await).await {
        Ok(count) => count,
        Err(error) => {
            tracing::error!(?error, "Failed to sync contacts; continuing");
            0
        }
    };
    let posthog_autofix_count =
        match report_job("posthog-autofix", process_posthog_autofix().await).await {
            Ok(count) => count,
            Err(error) => {
                tracing::error!(?error, "Failed to process PostHog auto-fix; continuing");
                0
            }
        };
    let message = format!(
        "Successfully processed {} emails, {} activity deadline reminders, {} estimate appointment reminders, {} maintenance due reminders, {} sms follow-ups, {} checklist surveys, {} call points, {} e-signature reminders, {} contact syncs, and {} PostHog auto-fix changes",
        processed_email_count,
        reminder_count,
        estimate_reminder_count,
        maintenance_reminder_count,
        sms_followup_count,
        checklist_survey_count,
        call_points_count,
        signature_reminder_count,
        contact_sync_count,
        posthog_autofix_count
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
    fn activity_reminder_needs_telegram_only_when_enabled_and_linked() {
        assert!(!activity_reminder_needs_telegram(false, None));
        assert!(!activity_reminder_needs_telegram(true, None));
        assert!(!activity_reminder_needs_telegram(false, Some(1)));
        assert!(activity_reminder_needs_telegram(true, Some(1)));
    }

    #[test]
    fn missing_telegram_token_does_not_fail_the_scheduled_tick() {
        let source = include_str!("generic_handler.rs");
        let fn_start = source
            .find("async fn send_due_activity_deadline_reminders")
            .expect("reminder function");
        let fn_end = source
            .find("/// Runs a scheduled job of the API")
            .expect("next function");
        let body = &source[fn_start..fn_end];
        assert!(
            body.contains(
                "Telegram notifications token is not set; skipping activity deadline reminders"
            ),
            "Missing Telegram token must skip reminders instead of failing the tick"
        );
        assert!(
            !body.contains("map_err(|error| Error::from(error.to_string()))"),
            "Missing Telegram token must not convert into a handler error"
        );
        assert!(
            source
                .contains("Failed to send activity deadline reminders; continuing scheduled work"),
            "Reminder failures must not abort SMS follow-ups"
        );
    }

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

    #[test]
    fn api_server_errors_are_not_reported_again() {
        assert_eq!(JobError::from_status(200), Ok(()));
        for status in [500, 502, 503, 599] {
            let error = JobError::from_status(status).expect_err("5xx fails");
            assert!(!error.should_report(), "{status}");
        }
        for status in [301, 400, 401, 404, 422, 429] {
            let error = JobError::from_status(status).expect_err("non-2xx fails");
            assert!(error.should_report(), "{status}");
            assert!(error.to_string().contains(&status.to_string()));
        }
    }

    #[tokio::test]
    async fn connection_errors_are_reported() {
        let error = Client::new()
            .post("http://127.0.0.1:1/v1/jobs/x")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(JobError::transport(&error).should_report());
    }

    #[test_db::test]
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
