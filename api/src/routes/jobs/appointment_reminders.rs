//! `POST /v1/jobs/appointment-reminders`: sends the due appointment
//! reminders. Customers get a text (else an email) 24 hours and 1 hour
//! before; reps get a CRM notification (pushed to their phones) and a
//! Telegram message the day before at 5 PM and 1 hour before. Then the
//! company's own notification rules (Admin > Schedule), which replace the
//! customer reminders of their calendars.
//!
//! Port of `processDueAppointmentReminders` and
//! `processDueAppointmentRepReminders`.

use crate::auth::job::JobCaller;
use crate::error::{ApiResult, ErrorBody};
use crate::openapi::ToSchema;
use crate::phone::crm_push::push_notification_logged;
use crate::routes::events::notify::{Booking, NotifyKind, appointment_when, notify_booking};
use crate::routes::schedule::notifications;
use crate::state::AppState;
use crate::webhooks_client::TelegramCrmNotify;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, NaiveDateTime, Utc};
use lambda_http::tracing;
use serde::Serialize;
use sqlx::MySqlPool;

/// What a run did.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct ReminderRun {
    /// Customer reminders that were due.
    pub processed: usize,
    /// Customer reminders sent.
    pub sent: usize,
    /// Customer reminders that failed (or came too late).
    pub failed: usize,
    /// Rep reminders that were due.
    pub rep_processed: usize,
    /// Rep reminders sent.
    pub rep_sent: usize,
    /// Rep reminders that failed.
    pub rep_failed: usize,
    /// Notification-rule messages that were due.
    pub rule_processed: usize,
    /// Notification-rule messages sent.
    pub rule_sent: usize,
    /// Notification-rule messages that failed.
    pub rule_failed: usize,
}

#[derive(sqlx::FromRow)]
struct CustomerReminder {
    id: i32,
    company_id: i32,
    customer_id: i32,
    reminder_kind: String,
    customer_name: Option<String>,
    customer_email: Option<String>,
    customer_phone: Option<String>,
    user_name: Option<String>,
    user_id: i32,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    location: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RepReminder {
    id: i32,
    user_id: i32,
    deal_id: Option<i32>,
    calendar_slug: String,
    reminder_kind: String,
    customer_name: Option<String>,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    location: Option<String>,
}

async fn mark(
    pool: &MySqlPool,
    table: &str,
    id: i32,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    let sql = match error {
        None => format!(
            "UPDATE {table} SET status = 'sent', sent_at = NOW(), error_message = NULL WHERE id = ?"
        ),
        Some(_) => format!("UPDATE {table} SET status = 'failed', error_message = ? WHERE id = ?"),
    };
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    if let Some(error) = error {
        query = query.bind(error.chars().take(255).collect::<String>());
    }
    query.bind(id).execute(pool).await?;
    Ok(())
}

fn customer_or_default(name: Option<&str>) -> &str {
    name.map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Customer")
}

async fn customer_reminders(state: &AppState, run: &mut ReminderRun) -> Result<(), sqlx::Error> {
    // The company's own flow on the event reminds instead (a step before it).
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE appointment_reminders r JOIN events e ON e.id = r.event_id
            SET r.status = 'cancelled', r.error_message = 'replaced_by_schedule_rule'
          WHERE r.status = 'pending' AND r.send_at <= UTC_TIMESTAMP()
            AND EXISTS (SELECT 1 FROM schedule_notification_flows f
                         WHERE f.company_id = r.company_id AND {}
                           AND EXISTS (SELECT 1 FROM schedule_notification_rules nr
                                        WHERE nr.flow_id = f.id AND nr.deleted_at IS NULL
                                          AND nr.direction = 'before'))",
        notifications::FLOW_APPLIES
    )))
    .execute(&state.pool)
    .await?;
    let due: Vec<CustomerReminder> = sqlx::query_as(
        "SELECT r.id, r.company_id, r.customer_id, CAST(r.reminder_kind AS CHAR) AS reminder_kind,
                c.name AS customer_name,
                COALESCE(NULLIF(TRIM(e.contact_email), ''), primary_email.email) AS customer_email,
                NULLIF(TRIM(e.contact_phone), '') AS customer_phone,
                creator.name AS user_name, e.created_user_id AS user_id,
                e.start_date, e.end_date, e.location
           FROM appointment_reminders r
           JOIN events e ON e.id = r.event_id
           JOIN users creator ON creator.id = e.created_user_id
           JOIN customers c ON c.id = r.customer_id
           LEFT JOIN customers_emails primary_email
             ON primary_email.id = c.email_id AND primary_email.customer_id = c.id
          WHERE r.status = 'pending' AND r.send_at <= UTC_TIMESTAMP() AND e.deleted_date IS NULL
          ORDER BY r.send_at, r.id LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await?;
    run.processed = due.len();
    let now = Utc::now();
    for reminder in due {
        let start: DateTime<Utc> = reminder.start_date.and_utc();
        if start <= now {
            mark(
                &state.pool,
                "appointment_reminders",
                reminder.id,
                Some("appointment_already_started"),
            )
            .await?;
            run.failed += 1;
            continue;
        }
        let booking = Booking {
            kind: if reminder.reminder_kind == "24h" {
                NotifyKind::Day
            } else {
                NotifyKind::Hour
            },
            company_id: reminder.company_id,
            customer_id: reminder.customer_id,
            customer_name: customer_or_default(reminder.customer_name.as_deref()),
            user_name: reminder.user_name.as_deref(),
            user_id: reminder.user_id,
            customer_email: reminder.customer_email.as_deref(),
            customer_phone: reminder.customer_phone.as_deref(),
            start,
            end: reminder.end_date.and_utc(),
            location: reminder.location.as_deref(),
        };
        let outcome = match notify_booking(state, &booking).await {
            Ok(Some(_)) => None,
            Ok(None) => Some("notification_unavailable".to_string()),
            Err(err) => {
                // One failure must not hold up the reminders queued behind it.
                tracing::error!(error = %err, reminder_id = reminder.id, "appointment reminder failed");
                Some("send_error".to_string())
            }
        };
        mark(
            &state.pool,
            "appointment_reminders",
            reminder.id,
            outcome.as_deref(),
        )
        .await?;
        if outcome.is_none() {
            run.sent += 1;
        } else {
            run.failed += 1;
        }
    }
    Ok(())
}

/// The calendar's reminder label and notification type.
fn calendar(slug: &str) -> (&'static str, &'static str) {
    match slug {
        "installations" => ("Installation", "installation_appointment_reminder"),
        "templates" => ("Template appointment", "template_appointment_reminder"),
        _ => ("In-home estimate", "estimate_appointment_reminder"),
    }
}

/// The CRM's `buildAppointmentRepReminderMessage`.
pub fn rep_message(
    slug: &str,
    kind: &str,
    customer: &str,
    when: &str,
    location: Option<&str>,
) -> String {
    let (label, _) = calendar(slug);
    let suffix = location
        .map(str::trim)
        .filter(|location| !location.is_empty())
        .map(|location| format!(" Address: {location}."))
        .unwrap_or_default();
    if kind == "day_before_5pm" {
        format!("{label} tomorrow with {customer} at {when}.{suffix}")
    } else {
        format!("{label} in 1 hour with {customer} at {when}.{suffix}")
    }
}

async fn rep_reminders(state: &AppState, run: &mut ReminderRun) -> Result<(), sqlx::Error> {
    let due: Vec<RepReminder> = sqlx::query_as(
        "SELECT r.id, r.user_id, r.deal_id, CAST(r.calendar_slug AS CHAR) AS calendar_slug,
                CAST(r.reminder_kind AS CHAR) AS reminder_kind, c.name AS customer_name,
                e.start_date, e.end_date, e.location
           FROM appointment_rep_reminders r
           JOIN events e ON e.id = r.event_id
           JOIN customers c ON c.id = r.customer_id
          WHERE r.status = 'pending' AND r.send_at <= UTC_TIMESTAMP() AND e.deleted_date IS NULL
          ORDER BY r.send_at, r.id LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await?;
    run.rep_processed = due.len();
    let now = Utc::now();
    for reminder in due {
        let start = reminder.start_date.and_utc();
        if start <= now {
            mark(
                &state.pool,
                "appointment_rep_reminders",
                reminder.id,
                Some("appointment_already_started"),
            )
            .await?;
            run.rep_failed += 1;
            continue;
        }
        let Some(deal_id) = reminder.deal_id else {
            mark(
                &state.pool,
                "appointment_rep_reminders",
                reminder.id,
                Some("notification_unavailable"),
            )
            .await?;
            run.rep_failed += 1;
            continue;
        };
        let customer = customer_or_default(reminder.customer_name.as_deref());
        let message: String = rep_message(
            &reminder.calendar_slug,
            &reminder.reminder_kind,
            customer,
            &appointment_when(start, reminder.end_date.and_utc()),
            reminder.location.as_deref(),
        )
        .chars()
        .take(255)
        .collect();
        let (_, notification_type) = calendar(&reminder.calendar_slug);
        let notification_id = sqlx::query(
            "INSERT INTO notifications (user_id, deal_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, ?, ?, NULL, NOW())",
        )
        .bind(reminder.user_id)
        .bind(deal_id)
        .bind(&message)
        .bind(notification_type)
        .execute(&state.pool)
        .await?
        .last_insert_id();
        push_notification_logged(state, i64::try_from(notification_id).unwrap_or_default()).await;
        state
            .webhooks
            .notify_telegram_crm(&TelegramCrmNotify {
                user_id: reminder.user_id,
                deal_id,
                notification_type: notification_type.to_string(),
                message,
                actor_name: None,
                customer_name: reminder
                    .customer_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
            })
            .await;
        mark(&state.pool, "appointment_rep_reminders", reminder.id, None).await?;
        run.rep_sent += 1;
    }
    Ok(())
}

/// Send due appointment reminders
///
/// Texts (else emails) customers their 24-hour and 1-hour reminders, and
/// notifies reps the day before and an hour before. Reminders for
/// appointments that already started are marked failed.
#[crate::openapi::path(
    post,
    path = "/v1/jobs/appointment-reminders",
    operation_id = "run_appointment_reminders",
    tag = "jobs",
    responses(
        (status = 200, description = "What was sent", body = ReminderRun),
        (status = 401, description = "Missing or wrong job key", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("job_key" = []))
)]
pub async fn run(State(state): State<AppState>, _job: JobCaller) -> ApiResult<Json<ReminderRun>> {
    let mut run = ReminderRun::default();
    customer_reminders(&state, &mut run).await?;
    rep_reminders(&state, &mut run).await?;
    let rules = notifications::send_due(&state).await?;
    run.rule_processed = rules.processed;
    run.rule_sent = rules.sent;
    run.rule_failed = rules.failed;
    Ok(Json(run))
}

#[cfg(test)]
mod tests {
    use super::rep_message;

    #[test]
    fn rep_messages_name_the_calendar() {
        assert_eq!(
            rep_message(
                "installations",
                "day_before_5pm",
                "Alice",
                "Tuesday, January 15 at 10:00 AM – 11:00 AM",
                Some(" 1 Main ")
            ),
            "Installation tomorrow with Alice at Tuesday, January 15 at 10:00 AM – 11:00 AM. Address: 1 Main."
        );
        assert_eq!(
            rep_message("estimates", "1h", "Bo", "now", None),
            "In-home estimate in 1 hour with Bo at now."
        );
    }
}
