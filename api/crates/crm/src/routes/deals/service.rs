//! Side effects shared by the deal writes: stage history, drip email/SMS
//! scheduling, reopening a closed deal and notifying the people on a deal.
//!
//! Ports of `app/crud/deals.ts`, `emailTemplateAutomation.server.ts`,
//! `smsTemplateAutomation.server.ts` and `lib/dealNotification.server.ts` from
//! the Remix app.

use crate::phone::crm_push::push_notification_logged;
use crate::state::AppState;
use crate::telegram::Bot;
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use common::crud::phone_app::has_app;
use common::telegram::crm::format_activity_notification;
use common::utils::email_send_window::{
    clamp_automated_email_send_at, clamp_automated_sms_send_at,
};
use lambda_http::tracing;
use sqlx::{MySqlConnection, MySqlPool};

/// Closes the open stage of `deal_id` and opens `list_id`.
pub async fn enter_stage(
    conn: &mut MySqlConnection,
    deal_id: i64,
    list_id: i32,
) -> Result<(), sqlx::Error> {
    close_stage(conn, deal_id).await?;
    sqlx::query("INSERT INTO deal_stage_history (deal_id, list_id) VALUES (?, ?)")
        .bind(deal_id)
        .bind(list_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A won deal still on the first stage of its pipeline (Not Contacted Yet)
/// moves to the next one (Contacted): the customer was obviously contacted.
/// Like a customer reply in the webhooks; no drips start, the deal is won.
pub async fn won_leaves_first_stage(
    conn: &mut MySqlConnection,
    deal_id: i64,
) -> Result<(), sqlx::Error> {
    let next: Option<(i32, String)> = sqlx::query_as(
        "SELECT nxt.id, nxt.name
           FROM deals d
           JOIN deals_list cur ON cur.id = d.list_id AND cur.deleted_at IS NULL
           JOIN deals_list nxt ON nxt.group_id = cur.group_id AND nxt.deleted_at IS NULL
                AND (nxt.position > cur.position OR (nxt.position = cur.position AND nxt.id > cur.id))
          WHERE d.id = ? AND d.deleted_at IS NULL AND d.is_won = 1
            AND NOT EXISTS (
                SELECT 1 FROM deals_list f
                 WHERE f.group_id = cur.group_id AND f.deleted_at IS NULL
                   AND (f.position < cur.position OR (f.position = cur.position AND f.id < cur.id)))
          ORDER BY nxt.position, nxt.id
          LIMIT 1",
    )
    .bind(deal_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((list_id, name)) = next else {
        return Ok(());
    };
    sqlx::query("UPDATE deals SET list_id = ?, status = ?, position = 0 WHERE id = ?")
        .bind(list_id)
        .bind(name)
        .bind(deal_id)
        .execute(&mut *conn)
        .await?;
    enter_stage(conn, deal_id, list_id).await
}

pub async fn close_stage(conn: &mut MySqlConnection, deal_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE deal_stage_history SET exited_at = NOW() WHERE deal_id = ? AND exited_at IS NULL",
    )
    .bind(deal_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Moves a deal to another stage: stage history, then drip emails and SMS of
/// the new list (`transitionDealStage`).
pub async fn transition_stage(
    pool: &MySqlPool,
    deal_id: i64,
    list_id: i32,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    enter_stage(&mut tx, deal_id, list_id).await?;
    tx.commit().await?;
    sync_drip(pool, deal_id, list_id, false).await
}

#[derive(sqlx::FromRow)]
#[allow(clippy::struct_field_names)]
struct DripContext {
    customer_id: i32,
    user_id: Option<i32>,
    company_id: i32,
    source: String,
}

#[derive(sqlx::FromRow)]
struct DripTemplate {
    id: i32,
    hour_delay: Option<i32>,
    minute_delay: i32,
    /// JSON array of `customers.source` keys; null: only leads.
    customer_sources: Option<String>,
}

impl DripTemplate {
    /// The customer's source gets this drip.
    fn reaches(&self, source: &str) -> bool {
        self.customer_sources
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .filter(|list| !list.is_empty())
            .map_or(source == "leads", |list| {
                list.iter().any(|key| key == source)
            })
    }
}

#[derive(Clone, Copy)]
enum DripChannel {
    Email,
    Sms,
}

impl DripChannel {
    const fn scheduled_table(self) -> &'static str {
        match self {
            Self::Email => "scheduled_emails",
            Self::Sms => "scheduled_sms",
        }
    }

    const fn template_table(self) -> &'static str {
        match self {
            Self::Email => "email_templates",
            Self::Sms => "cloudtalk_sms_templates",
        }
    }

    /// Email templates have no minutes or sources: leads only.
    const fn template_columns(self) -> &'static str {
        match self {
            Self::Email => "id, hour_delay, 0 AS minute_delay, NULL AS customer_sources",
            Self::Sms => {
                "id, hour_delay, minute_delay, CAST(customer_sources AS CHAR) AS customer_sources"
            }
        }
    }

    fn send_at(self, hour_delay: i32, minute_delay: i32) -> DateTime<Utc> {
        let when = Utc::now()
            + Duration::hours(hour_delay.into())
            + Duration::minutes(minute_delay.into());
        match self {
            Self::Email => clamp_automated_email_send_at(when),
            Self::Sms => clamp_automated_sms_send_at(when),
        }
    }
}

/// Cancels the deal's pending drips and schedules those of `list_id`.
///
/// Port of `syncDealListDripEmails` + `syncDealListDripSms`. Only deals of
/// customers with an owner get drips: leads, or for an SMS template the
/// sources it names; a new deal of a customer who already has another deal
/// gets none.
pub async fn sync_drip(
    pool: &MySqlPool,
    deal_id: i64,
    list_id: i32,
    new_deal: bool,
) -> Result<(), sqlx::Error> {
    for channel in [DripChannel::Email, DripChannel::Sms] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET status = 'cancelled', error_message = 'Deal moved lists' \
             WHERE deal_id = ? AND status = 'pending'",
            channel.scheduled_table()
        )))
        .bind(deal_id)
        .execute(pool)
        .await?;
    }

    let context: Option<DripContext> = sqlx::query_as(
        "SELECT d.customer_id, d.user_id, c.company_id,
                LOWER(TRIM(IFNULL(c.source, ''))) AS source
         FROM deals d
         JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND d.deleted_at IS NULL AND c.deleted_at IS NULL
         LIMIT 1",
    )
    .bind(deal_id)
    .fetch_optional(pool)
    .await?;
    let Some(context) = context else {
        return Ok(());
    };
    if new_deal {
        let other: Option<i64> = sqlx::query_scalar(
            "SELECT CAST(id AS SIGNED) FROM deals WHERE customer_id = ? AND id <> ? AND deleted_at IS NULL LIMIT 1",
        )
        .bind(context.customer_id)
        .bind(deal_id)
        .fetch_optional(pool)
        .await?;
        if other.is_some() {
            return Ok(());
        }
    }
    let Some(user_id) = context.user_id else {
        return Ok(());
    };

    for channel in [DripChannel::Email, DripChannel::Sms] {
        let templates: Vec<DripTemplate> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM {}
             WHERE company_id = ? AND user_id IS NULL AND lead_list_id = ? AND deleted_at IS NULL
             ORDER BY hour_delay ASC, id ASC",
            channel.template_columns(),
            channel.template_table()
        )))
        .bind(context.company_id)
        .bind(list_id)
        .fetch_all(pool)
        .await?;
        for template in templates {
            if !template.reaches(&context.source) {
                continue;
            }
            let table = channel.scheduled_table();
            let existing: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM {table} WHERE template_id = ? AND customer_id = ? \
                 AND status IN ('pending', 'sent') LIMIT 1"
            )))
            .bind(template.id)
            .bind(context.customer_id)
            .fetch_optional(pool)
            .await?;
            if existing.is_some() {
                continue;
            }
            let send_at = channel
                .send_at(template.hour_delay.unwrap_or(0), template.minute_delay)
                .naive_utc();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO {table}
                   (template_id, deal_id, list_id, customer_id, user_id, company_id, send_at, status)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')"
            )))
            .bind(template.id)
            .bind(deal_id)
            .bind(list_id)
            .bind(context.customer_id)
            .bind(user_id)
            .bind(context.company_id)
            .bind(send_at)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Reopens a closed deal: back to its list, or to the last live stage it was
/// in, or to the first stage of its group / the company's groups
/// (`reactivateDeal`). Then the list's drips are scheduled again.
pub async fn reopen(pool: &MySqlPool, deal_id: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let live_list: Option<i32> = sqlx::query_scalar(
        "SELECT d.list_id FROM deals d
         JOIN deals_list l ON l.id = d.list_id AND l.deleted_at IS NULL
         WHERE d.id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(deal_id)
    .fetch_optional(&mut *tx)
    .await?;

    let target = if let Some(list_id) = live_list {
        list_id
    } else {
        let from_history: Option<i32> = sqlx::query_scalar(
            "SELECT h.list_id FROM deal_stage_history h
             JOIN deals_list l ON l.id = h.list_id AND l.deleted_at IS NULL
             WHERE h.deal_id = ?
             ORDER BY h.entered_at DESC, h.id DESC LIMIT 1",
        )
        .bind(deal_id)
        .fetch_optional(&mut *tx)
        .await?;
        let target = match from_history {
            Some(list_id) => Some(list_id),
            None => {
                sqlx::query_scalar(
                    "SELECT dl.id FROM deals_list dl
                     JOIN groups_list gl ON dl.group_id = gl.id AND gl.deleted_at IS NULL AND gl.is_displayed = 1
                     JOIN deals d ON d.id = ?
                     JOIN customers c ON d.customer_id = c.id AND (gl.company_id = c.company_id OR gl.id = 1)
                     LEFT JOIN deals_list old ON old.id = d.list_id
                     WHERE dl.deleted_at IS NULL
                     ORDER BY gl.id = old.group_id DESC, gl.company_id = c.company_id DESC,
                              gl.is_default DESC, gl.id ASC, dl.position ASC LIMIT 1",
                )
                .bind(deal_id)
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        let Some(target) = target else {
            tx.commit().await?;
            return Ok(());
        };
        sqlx::query("UPDATE deals SET list_id = ? WHERE id = ?")
            .bind(target)
            .bind(deal_id)
            .execute(&mut *tx)
            .await?;
        target
    };
    enter_stage(&mut tx, deal_id, target).await?;
    tx.commit().await?;
    sync_drip(pool, deal_id, target, false).await
}

/// Kinds of deal notifications (the `notifications.notification_type` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notification {
    ActivityAdded,
    ActivityEdited,
    ActivityDeleted,
    NoteAdded,
    NoteEdited,
    NoteDeleted,
    CommentAdded,
    CommentDeleted,
}

impl Notification {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActivityAdded => "activity_added",
            Self::ActivityEdited => "activity_edited",
            Self::ActivityDeleted => "activity_deleted",
            Self::NoteAdded => "note_added",
            Self::NoteEdited => "note_edited",
            Self::NoteDeleted => "note_deleted",
            Self::CommentAdded => "comment_added",
            Self::CommentDeleted => "comment_deleted",
        }
    }
}

/// First 255 characters, the size of `notifications.message`.
fn clip_message(message: &str) -> String {
    message.chars().take(255).collect()
}

#[derive(sqlx::FromRow)]
struct DealPeople {
    user_id: Option<i32>,
    sales_rep: Option<i32>,
    customer_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TelegramTarget {
    notifications_telegram_id: Option<i64>,
    telegram_activity_notifications: bool,
}

/// Tells the deal's owner and the customer's sales rep (not the actor).
///
/// A `notifications` row each, pushed to their phones, and a Telegram
/// message when they linked the notifications bot (`notifyDealAssignee`).
/// Failures are logged, never
/// returned: the write that triggered it already succeeded.
pub async fn notify_assignees(
    state: &AppState,
    deal_id: i64,
    actor_id: i32,
    actor_name: &str,
    message: &str,
    kind: Notification,
) {
    if let Err(error) = try_notify(state, deal_id, actor_id, actor_name, message, kind).await {
        tracing::error!(?error, deal_id, "deal notification failed");
    }
}

async fn try_notify(
    state: &AppState,
    deal_id: i64,
    actor_id: i32,
    actor_name: &str,
    message: &str,
    kind: Notification,
) -> Result<(), sqlx::Error> {
    let people: Option<DealPeople> = sqlx::query_as(
        "SELECT d.user_id, c.sales_rep, c.name AS customer_name
         FROM deals d JOIN customers c ON c.id = d.customer_id
         WHERE d.id = ? AND d.deleted_at IS NULL LIMIT 1",
    )
    .bind(deal_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(people) = people else {
        return Ok(());
    };
    let mut recipients: Vec<i32> = Vec::new();
    for id in [people.user_id, people.sales_rep].into_iter().flatten() {
        if id != actor_id && !recipients.contains(&id) {
            recipients.push(id);
        }
    }
    let message = clip_message(message);
    for recipient in recipients {
        let notification_id = sqlx::query(
            "INSERT INTO notifications (user_id, deal_id, message, notification_type, actor_name, due_at)
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(recipient)
        .bind(deal_id)
        .bind(&message)
        .bind(kind.as_str())
        .bind(actor_name)
        .execute(&state.pool)
        .await?
        .last_insert_id();
        push_notification_logged(state, i64::try_from(notification_id).unwrap_or_default()).await;

        let target: Option<TelegramTarget> = sqlx::query_as(
            "SELECT notifications_telegram_id, telegram_activity_notifications
             FROM users WHERE id = ? AND is_deleted = 0",
        )
        .bind(recipient)
        .fetch_optional(&state.pool)
        .await?;
        let Some(TelegramTarget {
            notifications_telegram_id: Some(chat_id),
            telegram_activity_notifications: true,
        }) = target
        else {
            continue;
        };
        // The app has it already.
        if has_app(&state.pool, recipient).await {
            continue;
        }
        let text = format_activity_notification(
            kind.as_str(),
            people.customer_name.as_deref(),
            Some(actor_name),
            &message,
            i32::try_from(deal_id).unwrap_or_default(),
        );
        state
            .telegram
            .send_with_button(
                Bot::Notifications,
                chat_id,
                &text.text,
                text.button_label,
                &text.button_url,
            )
            .await;
    }
    Ok(())
}

/// Deletes the pending deadline reminder of an activity.
pub async fn remove_activity_reminder(
    pool: &MySqlPool,
    deal_id: i64,
    activity_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM notifications
         WHERE deal_id = ? AND notification_type = 'activity_deadline_reminder' AND message = ?",
    )
    .bind(deal_id)
    .bind(clip_message(activity_name))
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces the deadline reminder of an activity.
///
/// `old_name` is its name before an edit. The reminder goes to the deal's
/// owner, or the actor when the deal has none, at the deadline. Date-only
/// deadlines get no reminder.
pub async fn schedule_activity_reminder(
    pool: &MySqlPool,
    deal_id: i64,
    actor_id: i32,
    name: &str,
    deadline: Option<&str>,
    old_name: Option<&str>,
) -> Result<(), sqlx::Error> {
    remove_activity_reminder(pool, deal_id, old_name.unwrap_or(name)).await?;
    let Some(due_at) = deadline.and_then(reminder_due_at) else {
        return Ok(());
    };
    let owner: Option<Option<i32>> =
        sqlx::query_scalar("SELECT user_id FROM deals WHERE id = ? AND deleted_at IS NULL LIMIT 1")
            .bind(deal_id)
            .fetch_optional(pool)
            .await?;
    let recipient = owner.flatten().unwrap_or(actor_id);
    sqlx::query(
        "INSERT INTO notifications (user_id, deal_id, message, notification_type, actor_name, due_at)
         VALUES (?, ?, ?, 'activity_deadline_reminder', NULL, ?)",
    )
    .bind(recipient)
    .bind(deal_id)
    .bind(clip_message(name))
    .bind(due_at.naive_utc())
    .execute(pool)
    .await?;
    Ok(())
}

/// When a deadline reminder fires: the deadline as an instant. Deadlines are
/// store wall times (`2026-05-01T14:30:00`) unless they carry `Z` or an
/// offset. Midnight means "no time", which gets no reminder.
pub fn reminder_due_at(deadline: &str) -> Option<DateTime<Utc>> {
    let deadline = deadline.trim();
    if deadline.len() < 19 {
        return None;
    }
    let wall =
        NaiveDateTime::parse_from_str(&deadline[..19].replace(' ', "T"), "%Y-%m-%dT%H:%M:%S")
            .ok()?;
    if wall.time() == chrono::NaiveTime::MIN {
        return None;
    }
    let rest = &deadline[19..];
    let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if rest.eq_ignore_ascii_case("z") {
        return Some(Utc.from_utc_datetime(&wall));
    }
    if !rest.is_empty() {
        return DateTime::parse_from_rfc3339(deadline)
            .ok()
            .map(|at| at.with_timezone(&Utc));
    }
    Indianapolis
        .from_local_datetime(&wall)
        .earliest()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::reminder_due_at;

    #[test]
    fn reminder_times() {
        let at = |s: &str| reminder_due_at(s).map(|d| d.to_rfc3339());
        assert_eq!(
            at("2026-05-01T14:30:00"),
            Some("2026-05-01T18:30:00+00:00".into())
        );
        assert_eq!(
            at("2026-05-01 14:30:00"),
            Some("2026-05-01T18:30:00+00:00".into())
        );
        assert_eq!(
            at("2026-12-01T14:30:00"),
            Some("2026-12-01T19:30:00+00:00".into())
        );
        assert_eq!(
            at("2026-05-01T14:30:00.000Z"),
            Some("2026-05-01T14:30:00+00:00".into())
        );
        assert_eq!(
            at("2026-05-01T14:30:00-04:00"),
            Some("2026-05-01T18:30:00+00:00".into())
        );
        assert_eq!(at("2026-05-01T00:00:00"), None);
        assert_eq!(at("2026-05-01"), None);
        assert_eq!(at("garbage"), None);
    }
}
