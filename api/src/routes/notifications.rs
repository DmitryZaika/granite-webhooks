//! The notification bell.
//!
//! - `GET /v1/notifications` → unread mail, activity and billing
//!   notifications, and missed `CloudTalk` and `RingCentral` calls, newest first
//! - `POST /v1/notifications/done` → dismiss notifications, or all of them
//!
//! Replaces the Remix `api.notifications` loader and `api.notifications.done`
//! action. The Remix loader also sent due Telegram activity reminders on each
//! poll; the `time-triggered` Lambda sends those every minute.

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, QueryBuilder};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(done))
}

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationKind {
    Activity,
    Email,
    Call,
    Billing,
}

/// One notification of the bell.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NotificationRef {
    /// `notif-<id>` for activities, `billing-<id>` for billing, the thread
    /// id for email. Calls are dismissed in the browser only.
    pub id: String,
    pub kind: NotificationKind,
}

/// Body of `POST /v1/notifications/done`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DoneInput {
    /// Dismiss everything: activity and billing notifications, and every
    /// unread incoming email.
    #[serde(default)]
    pub clear_all: bool,
    /// The notifications to dismiss.
    #[serde(default)]
    pub items: Vec<NotificationRef>,
    /// Remove email threads from the bell without marking them read. They
    /// show again when a newer message arrives.
    #[serde(default)]
    pub hide_only: bool,
}

fn numbered(items: &[NotificationRef], kind: NotificationKind, prefix: &str) -> Vec<i64> {
    items
        .iter()
        .filter(|item| item.kind == kind)
        .filter_map(|item| {
            item.id
                .strip_prefix(prefix)
                .unwrap_or(&item.id)
                .parse()
                .ok()
        })
        .filter(|id| *id > 0)
        .collect()
}

/// Pushes the CRM's `dismissIncomingEmailSql`: customer mail to the user or
/// on their deal, and coworkers' mail addressed to them.
fn push_incoming(builder: &mut QueryBuilder<MySql>, user: &SessionUser) {
    let email = user.email.trim().to_lowercase();
    let like = format!("%<{email}>");
    let addressed = |builder: &mut QueryBuilder<MySql>| {
        builder
            .push("e.receiver_user_id = ")
            .push_bind(user.id)
            .push(" OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))) = ")
            .push_bind(email.clone())
            .push(" OR (e.receiver_email NOT LIKE '%<%' AND LOWER(TRIM(e.receiver_email)) = ")
            .push_bind(email.clone())
            .push(") OR e.receiver_email LIKE ")
            .push_bind(like.clone());
    };
    builder
        .push("((e.sender_user_id IS NULL AND (d.user_id = ")
        .push_bind(user.id)
        .push(" OR ");
    addressed(builder);
    builder
        .push(")) OR (e.sender_user_id IS NOT NULL AND e.sender_user_id <> ")
        .push_bind(user.id)
        .push(" AND (");
    addressed(builder);
    builder.push(")))");
}

/// Dismiss notifications
///
/// Marks activity and billing notifications done and the listed email
/// threads read. With `clear_all`, dismisses every one of them and marks all
/// unread incoming mail read.
#[utoipa::path(
    post,
    path = "/v1/notifications/done",
    operation_id = "dismiss_notifications",
    tag = "notifications",
    request_body = DoneInput,
    responses(
        (status = 204, description = "Dismissed"),
        (status = 400, description = "Nothing to dismiss", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn done(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(input): JsonBody<DoneInput>,
) -> ApiResult<StatusCode> {
    let threads: Vec<&str> = input
        .items
        .iter()
        .filter(|item| item.kind == NotificationKind::Email && !item.id.is_empty())
        .map(|item| item.id.as_str())
        .collect();
    let activities = numbered(&input.items, NotificationKind::Activity, "notif-");
    let billing = numbered(&input.items, NotificationKind::Billing, "billing-");
    if !input.clear_all && input.items.is_empty() {
        return Err(ApiError::BadRequest("notification target required".into()));
    }
    let mut tx = state.pool.begin().await?;
    for (table, ids) in [
        ("notifications", &activities),
        ("billing_notifications", &billing),
    ] {
        if !input.clear_all && ids.is_empty() {
            continue;
        }
        let mut builder =
            QueryBuilder::<MySql>::new(format!("UPDATE {table} SET is_done = 1 WHERE user_id = "));
        builder.push_bind(user.id);
        if !input.clear_all {
            builder.push(" AND id IN (");
            let mut list = builder.separated(", ");
            for id in ids {
                list.push_bind(*id);
            }
            builder.push(")");
        }
        builder.build().execute(&mut *tx).await?;
    }
    if input.hide_only {
        let listed;
        let threads: Vec<&str> = if input.clear_all {
            listed = unread_email(&state, &user).await?;
            listed.iter().map(|item| item.id.as_str()).collect()
        } else {
            threads
        };
        if !threads.is_empty() {
            let mut builder = QueryBuilder::<MySql>::new(
                "INSERT INTO notification_email_hides (user_id, thread_id, hidden_at) ",
            );
            builder.push_values(&threads, |mut row, thread| {
                row.push_bind(user.id).push_bind(*thread).push("NOW()");
            });
            builder.push(" ON DUPLICATE KEY UPDATE hidden_at = VALUES(hidden_at)");
            builder.build().execute(&mut *tx).await?;
        }
    } else if input.clear_all || !threads.is_empty() {
        let mut builder = QueryBuilder::<MySql>::new(
            "UPDATE emails e LEFT JOIN deals d ON d.id = e.deal_id AND d.deleted_at IS NULL
                SET e.employee_read_at = NOW()
              WHERE e.deleted_at IS NULL AND e.thread_id IS NOT NULL AND e.employee_read_at IS NULL AND ",
        );
        if !input.clear_all {
            builder.push("e.thread_id IN (");
            let mut list = builder.separated(", ");
            for thread in &threads {
                list.push_bind(*thread);
            }
            builder.push(") AND ");
        }
        push_incoming(&mut builder, &user);
        builder.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Onboarding sample threads and the sample actor, hidden outside the tour.
const SAMPLE_EMAIL_THREAD: &str = "onboarding-sample-thread";
const SAMPLE_NOTIFICATION_THREAD: &str = "onboarding-sample-notif-thread";
const SAMPLE_ACTOR: &str = "Onboarding Guide";
/// `notifications.actor_name` once the Telegram reminder went out.
const TELEGRAM_SENT_MARKER: &str = "__telegram_sent__";

/// A notification of the bell.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationItem {
    /// `notif-<id>`, `billing-<id>`, `call-<id>` (`CloudTalk`), `rc-call-<id>`
    /// (`RingCentral`), or the email thread id.
    pub id: String,
    /// Usually the customer's name.
    pub title: String,
    /// What happened.
    pub message: String,
    /// Where it opens.
    pub href: String,
    /// When (ISO 8601, UTC).
    pub sent_at: String,
    pub kind: NotificationKind,
    /// The activity notification's type (`note_added`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    /// Who did it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_name: Option<String>,
    /// The customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_name: Option<String>,
    /// A heading for the type (`Added a Note`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_title: Option<String>,
    /// Billing: the button text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_label: Option<String>,
}

/// `GET /v1/notifications`.
#[derive(Debug, Serialize, ToSchema)]
pub struct Notifications {
    /// Newest first.
    pub notifications: Vec<NotificationItem>,
}

/// Query of `GET /v1/notifications`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// The onboarding tour is open: keep its sample notifications.
    #[serde(default)]
    pub samples: bool,
}

fn type_title(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "activity_added" => "Added an Activity",
        "activity_edited" => "Edited an Activity",
        "activity_deleted" => "Deleted an Activity",
        "activity_deadline_reminder" => "Activity Reminder",
        "estimate_appointment_reminder" => "In-Home Estimate Reminder",
        "installation_appointment_reminder" => "Installation Reminder",
        "template_appointment_reminder" => "Template Appointment Reminder",
        "note_added" => "Added a Note",
        "note_edited" => "Edited a Note",
        "note_deleted" => "Deleted a Note",
        "comment_added" => "Added a Comment",
        "comment_deleted" => "Deleted a Comment",
        "signature_viewed" => "Document Opened for Signing",
        "signature_signed" => "Document Signed",
        "signature_declined" => "Document Declined",
        "quote_signed" => "Quote Signed",
        "delayed_message_sent" => "Scheduled Message Sent",
        "delayed_message_failed" => "Scheduled Message Failed",
        _ => return None,
    })
}

#[derive(sqlx::FromRow)]
struct EmailRow {
    thread_id: String,
    deal_id: Option<i64>,
    subject: Option<String>,
    sent_at: String,
    customer_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ActivityRow {
    id: i64,
    deal_id: Option<i64>,
    customer_id: i32,
    message: String,
    customer_name: Option<String>,
    created_at: String,
    notification_type: Option<String>,
    actor_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct BillingRow {
    id: i32,
    title: String,
    message: String,
    action_label: String,
    href: String,
    created_at: String,
}

async fn unread_email(
    state: &AppState,
    user: &SessionUser,
) -> Result<Vec<NotificationItem>, sqlx::Error> {
    let email = user.email.trim().to_lowercase();
    let like = format!("%<{email}>");
    let rows: Vec<EmailRow> = sqlx::query_as(
        "SELECT e.thread_id, CAST(COALESCE(e.deal_id, td.deal_id) AS SIGNED) AS deal_id, e.subject,
                DATE_FORMAT(e.sent_at, '%Y-%m-%dT%H:%i:%sZ') AS sent_at, c.name AS customer_name
           FROM emails e
           JOIN (
             SELECT thread_id, MAX(sent_at) AS max_sent_at FROM emails
              WHERE deleted_at IS NULL AND thread_id IS NOT NULL AND sent_at > NOW() - INTERVAL 14 DAY
                AND (sender_user_id IS NULL OR (sender_user_id <> ? AND (
                      receiver_user_id = ?
                      OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(receiver_email, '<', -1), '>', 1))) = ?
                      OR (receiver_email NOT LIKE '%<%' AND LOWER(TRIM(receiver_email)) = ?)
                      OR receiver_email LIKE ?)))
              GROUP BY thread_id
           ) last_e ON last_e.thread_id = e.thread_id AND last_e.max_sent_at = e.sent_at
           LEFT JOIN (
             SELECT thread_id, MAX(deal_id) AS deal_id FROM emails
              WHERE deleted_at IS NULL AND thread_id IS NOT NULL AND deal_id IS NOT NULL
                AND sent_at > NOW() - INTERVAL 14 DAY
              GROUP BY thread_id
           ) td ON td.thread_id = e.thread_id
           LEFT JOIN deals d ON d.id = COALESCE(e.deal_id, td.deal_id) AND d.deleted_at IS NULL
           LEFT JOIN customers c ON c.id = d.customer_id
          WHERE e.deleted_at IS NULL AND e.thread_id IS NOT NULL AND e.employee_read_at IS NULL
            AND e.sent_at > NOW() - INTERVAL 14 DAY
            AND (e.sender_user_id IS NULL OR e.sender_user_id <> ?)
            AND (e.receiver_user_id = ? OR d.user_id = ?
                 OR LOWER(TRIM(SUBSTRING_INDEX(SUBSTRING_INDEX(e.receiver_email, '<', -1), '>', 1))) = ?
                 OR (e.receiver_email NOT LIKE '%<%' AND LOWER(TRIM(e.receiver_email)) = ?)
                 OR e.receiver_email LIKE ?)
            AND NOT EXISTS (
                  SELECT 1 FROM notification_email_hides h
                   WHERE h.user_id = ? AND h.thread_id = e.thread_id AND h.hidden_at >= e.sent_at)
          ORDER BY e.sent_at DESC LIMIT 50",
    )
    .bind(user.id)
    .bind(user.id)
    .bind(&email)
    .bind(&email)
    .bind(&like)
    .bind(user.id)
    .bind(user.id)
    .bind(user.id)
    .bind(&email)
    .bind(&email)
    .bind(&like)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|row| !row.thread_id.is_empty())
        .map(|row| {
            let href = match row.deal_id.filter(|id| *id > 0) {
                Some(deal) => format!("/employee/deals/edit/{deal}/project/chat/{}", row.thread_id),
                None => format!("/employee/emails/chat/{}", row.thread_id),
            };
            NotificationItem {
                title: row
                    .customer_name
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "New email".into()),
                message: row.subject.unwrap_or_else(|| "New email".into()),
                href,
                sent_at: row.sent_at,
                kind: NotificationKind::Email,
                id: row.thread_id,
                notification_type: None,
                actor_name: None,
                customer_name: None,
                type_title: None,
                action_label: None,
            }
        })
        .collect())
}

async fn activity(state: &AppState, user_id: i32) -> Result<Vec<NotificationItem>, sqlx::Error> {
    let rows: Vec<ActivityRow> = sqlx::query_as(
        "SELECT CAST(n.id AS SIGNED) AS id, CAST(n.deal_id AS SIGNED) AS deal_id, c.id AS customer_id, n.message,
                c.name AS customer_name, n.notification_type, n.actor_name,
                -- A deadline reminder happens at its deadline, not when it was scheduled.
                DATE_FORMAT(CASE WHEN n.notification_type = 'activity_deadline_reminder'
                                 THEN n.due_at ELSE n.created_at END,
                            '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM notifications n
           LEFT JOIN deals d ON d.id = n.deal_id AND d.deleted_at IS NULL
           JOIN customers c ON c.id = COALESCE(d.customer_id, n.customer_id)
          WHERE n.user_id = ? AND n.is_done = 0
            AND d.id IS NOT NULL
            -- Signature status is emailed to the sender, so it is not listed here.
            AND (n.notification_type IS NULL OR n.notification_type NOT IN
                 ('signature_viewed', 'signature_signed', 'signature_declined'))
            -- Deadline reminders stay until dismissed or the activity is completed.
            AND CASE WHEN n.notification_type = 'activity_deadline_reminder'
                     THEN n.due_at <= UTC_TIMESTAMP()
                     ELSE n.due_at <= NOW() END
            AND (n.notification_type != 'activity_deadline_reminder' OR EXISTS (
                  SELECT 1 FROM deal_activities da
                   WHERE da.deal_id = n.deal_id AND da.deleted_at IS NULL AND da.is_completed = 0
                     AND da.deadline IS NOT NULL AND TIME(da.deadline) <> '00:00:00'
                     AND LEFT(da.name, 255) = n.message))
          ORDER BY created_at DESC LIMIT 50",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let customer = row.customer_name.unwrap_or_default();
            NotificationItem {
                id: format!("notif-{}", row.id),
                title: customer.clone(),
                message: row.message,
                href: row.deal_id.map_or_else(
                    || format!("/employee/customers/info/{}/documents", row.customer_id),
                    |deal| format!("/employee/deals/edit/{deal}/project"),
                ),
                sent_at: row.created_at,
                kind: NotificationKind::Activity,
                type_title: row
                    .notification_type
                    .as_deref()
                    .and_then(type_title)
                    .map(str::to_string),
                notification_type: row.notification_type,
                actor_name: row.actor_name.filter(|actor| actor != TELEGRAM_SENT_MARKER),
                customer_name: Some(customer),
                action_label: None,
            }
        })
        .collect())
}

async fn billing(state: &AppState, user_id: i32) -> Result<Vec<NotificationItem>, sqlx::Error> {
    let rows: Vec<BillingRow> = sqlx::query_as(
        "SELECT id, title, message, action_label, href,
                DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%sZ') AS created_at
           FROM billing_notifications WHERE user_id = ? AND is_done = 0
          ORDER BY created_at DESC LIMIT 20",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| NotificationItem {
            id: format!("billing-{}", row.id),
            title: row.title,
            message: row.message,
            href: row.href,
            sent_at: row.created_at,
            kind: NotificationKind::Billing,
            notification_type: None,
            actor_name: None,
            customer_name: None,
            type_title: None,
            action_label: Some(row.action_label),
        })
        .collect())
}

/// Missed calls and voicemail of the last three days on the caller's
/// `CloudTalk` line and `RingCentral` extension.
async fn missed_calls(state: &AppState, user: &SessionUser) -> Vec<NotificationItem> {
    use crate::routes::users::call_activity::{
        CallActivity, activity, agent_calls, company_ringcentral_calls, ringcentral_activity,
    };
    let present = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let since = chrono::Utc::now() - chrono::Duration::days(3);
    let cloudtalk = async {
        let Some(agent) = present(&user.cloudtalk_agent_id) else {
            return Vec::new();
        };
        let from = since.format("%Y-%m-%d").to_string();
        agent_calls(state, user.company_id, &agent, &from)
            .await
            .iter()
            .map(|item| activity(item, user.id, ""))
            .collect::<Vec<CallActivity>>()
    };
    let ringcentral = async {
        let Some(extension) = present(&user.ringcentral_extension_id) else {
            return Vec::new();
        };
        let from = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        company_ringcentral_calls(state, user.company_id, Some(&extension), &from)
            .await
            .iter()
            .map(|record| ringcentral_activity(record, user.id, ""))
            .collect::<Vec<CallActivity>>()
    };
    let (cloudtalk, ringcentral) = tokio::join!(cloudtalk, ringcentral);
    cloudtalk
        .into_iter()
        .chain(ringcentral)
        .filter(|call| call.action == "Missed a call")
        .take(30)
        .map(|call| missed_call_item(&call))
        .collect()
}

fn missed_call_item(call: &crate::routes::users::call_activity::CallActivity) -> NotificationItem {
    let ringcentral = call.source == "ringcentral_call";
    let (provider, path) = if ringcentral {
        ("RingCentral", "ringcentral")
    } else {
        ("CloudTalk", "cloudtalk")
    };
    let external = call.contact.clone().unwrap_or_default();
    let digits: String = external.chars().filter(char::is_ascii_digit).collect();
    let digits = &digits[digits.len().saturating_sub(10)..];
    let started = call.created_at.as_str();
    // CloudTalk sends `2026-05-01 10:00:00` (UTC), RingCentral RFC 3339.
    let sent_at = chrono::NaiveDateTime::parse_from_str(started, "%Y-%m-%d %H:%M:%S")
        .map(|at| at.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc3339(started).map(|at| at.with_timezone(&chrono::Utc))
        })
        .map_or_else(
            |_| started.to_string(),
            |at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
    NotificationItem {
        id: if ringcentral {
            format!("rc-call-{}", call.call_id)
        } else {
            format!("call-{}", call.call_id)
        },
        title: call.customer_name.clone().unwrap_or_else(|| {
            if external.is_empty() {
                "Unknown caller".into()
            } else {
                external.clone()
            }
        }),
        message: if call.is_voicemail {
            "New voicemail"
        } else {
            "Missed call"
        }
        .into(),
        href: if digits.len() >= 10 {
            format!("/employee/{path}/thread/{digits}")
        } else {
            format!("/employee/{path}")
        },
        sent_at,
        kind: NotificationKind::Call,
        notification_type: None,
        actor_name: None,
        customer_name: call.customer_name.clone(),
        type_title: Some(if call.is_voicemail {
            format!("{provider} voicemail")
        } else {
            format!("Missed {provider} call")
        }),
        action_label: None,
    }
}

/// List notifications
///
/// Unread incoming mail of the last 14 days (one per conversation), due
/// notifications on live deals (signature status is emailed to the sender
/// instead, so it is left out), open billing notices and missed
/// `CloudTalk` and `RingCentral` calls, newest first. The onboarding tour's sample
/// notifications are left out unless `samples` is set.
#[utoipa::path(
    get,
    path = "/v1/notifications",
    operation_id = "list_notifications",
    tag = "notifications",
    params(ListQuery),
    responses(
        (status = 200, description = "Notifications", body = Notifications),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Notifications>> {
    let (email, activities, bills, calls) = tokio::join!(
        unread_email(&state, &user),
        activity(&state, user.id),
        billing(&state, user.id),
        missed_calls(&state, &user),
    );
    let sample_email = format!("{SAMPLE_EMAIL_THREAD}-{}", user.company_id);
    let sample_notification = format!("{SAMPLE_NOTIFICATION_THREAD}-{}", user.company_id);
    let mut notifications: Vec<NotificationItem> = email?
        .into_iter()
        .chain(activities?)
        .chain(calls)
        .chain(bills?)
        .filter(|item| {
            query.samples
                || match item.kind {
                    NotificationKind::Email => {
                        !item.id.starts_with(&sample_email)
                            && !item.id.starts_with(&sample_notification)
                    }
                    NotificationKind::Activity => item.actor_name.as_deref() != Some(SAMPLE_ACTOR),
                    _ => true,
                }
        })
        .collect();
    let at = |item: &NotificationItem| chrono::DateTime::parse_from_rfc3339(&item.sent_at).ok();
    notifications.sort_by_key(|item| std::cmp::Reverse(at(item)));
    Ok(Json(Notifications { notifications }))
}
