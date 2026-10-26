//! Appointment reminders of events on the built-in calendars.
//!
//! Texts or emails to the customer (`appointment_reminders`, 24 hours and 1
//! hour before) and Telegram nudges to the rep (`appointment_rep_reminders`,
//! 5 PM store time the day before and 1 hour before). The cron sends them;
//! this keeps their send times in step with the event.
//!
//! Port of `app/utils/appointmentReminders.server.ts`,
//! `appointmentRepReminders.server.ts` and `eventReminderResync.server.ts`.

use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use chrono_tz::America::Indiana::Indianapolis;
use sqlx::MySqlPool;

/// Calendars whose events remind the customer and rep.
const REMINDER_CALENDARS: [&str; 3] = ["estimates", "installations", "templates"];

fn mysql(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// `24h` / `1h` send times still in the future for an appointment at `start`.
pub fn customer_send_times(
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> [(&'static str, Option<DateTime<Utc>>); 2] {
    let until = start - now;
    [
        (
            "24h",
            (until >= Duration::days(1)).then(|| start - Duration::days(1)),
        ),
        (
            "1h",
            (until >= Duration::hours(1)).then(|| start - Duration::hours(1)),
        ),
    ]
}

/// `day_before_5pm` / `1h` send times still in the future for the rep.
pub fn rep_send_times(
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> [(&'static str, Option<DateTime<Utc>>); 2] {
    let day_before = (start > now)
        .then(|| {
            let store_day = start.with_timezone(&Indianapolis).date_naive() - Duration::days(1);
            Indianapolis
                .from_local_datetime(&store_day.and_hms_opt(17, 0, 0)?)
                .earliest()
                .map(|at| at.with_timezone(&Utc))
        })
        .flatten()
        .filter(|at| *at > now);
    [
        ("day_before_5pm", day_before),
        (
            "1h",
            (start - now >= Duration::hours(1)).then(|| start - Duration::hours(1)),
        ),
    ]
}

/// The event's built-in calendar slug when it is one that sends reminders.
pub async fn eligible_slug(
    pool: &MySqlPool,
    event_id: i32,
    company_id: i32,
) -> Result<Option<String>, sqlx::Error> {
    let slug: Option<Option<String>> = sqlx::query_scalar(
        "SELECT cal.slug FROM events e JOIN calendars cal ON cal.id = e.calendar_id
          WHERE e.id = ? AND cal.company_id = ? AND e.deleted_date IS NULL LIMIT 1",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(slug
        .flatten()
        .filter(|slug| REMINDER_CALENDARS.contains(&slug.as_str())))
}

/// Who and what an event's reminders are for.
pub struct Appointment<'a> {
    pub event_id: i32,
    pub company_id: i32,
    pub customer_id: i32,
    pub slug: &'a str,
    pub start: DateTime<Utc>,
}

/// Sets the customer reminders' send times; a time already past cancels it.
pub async fn schedule_customer(
    pool: &MySqlPool,
    appointment: &Appointment<'_>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    for (kind, send_at) in customer_send_times(appointment.start, now) {
        if let Some(send_at) = send_at {
            sqlx::query(
                "INSERT INTO appointment_reminders
                   (event_id, company_id, customer_id, calendar_slug, reminder_kind, send_at, status)
                 VALUES (?, ?, ?, ?, ?, ?, 'pending')
                 ON DUPLICATE KEY UPDATE company_id = VALUES(company_id),
                   customer_id = VALUES(customer_id), calendar_slug = VALUES(calendar_slug),
                   send_at = VALUES(send_at), status = 'pending', sent_at = NULL, error_message = NULL",
            )
            .bind(appointment.event_id)
            .bind(appointment.company_id)
            .bind(appointment.customer_id)
            .bind(appointment.slug)
            .bind(kind)
            .bind(mysql(send_at))
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE appointment_reminders SET status = 'cancelled', error_message = NULL
                  WHERE event_id = ? AND reminder_kind = ? AND status = 'pending'",
            )
            .bind(appointment.event_id)
            .bind(kind)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Sets the rep reminders' send times; a time already past cancels it.
/// Without `deal_id` the customer's newest deal is linked.
pub async fn schedule_rep(
    pool: &MySqlPool,
    appointment: &Appointment<'_>,
    user_id: i32,
    deal_id: Option<i32>,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let deal_id: Option<i64> = match deal_id.filter(|id| *id > 0) {
        Some(id) => Some(i64::from(id)),
        None => {
            sqlx::query_scalar(
                "SELECT CAST(id AS SIGNED) FROM deals WHERE customer_id = ? AND deleted_at IS NULL ORDER BY id DESC LIMIT 1",
            )
            .bind(appointment.customer_id)
            .fetch_optional(pool)
            .await?
        }
    };
    for (kind, send_at) in rep_send_times(appointment.start, now) {
        if let Some(send_at) = send_at {
            sqlx::query(
                "INSERT INTO appointment_rep_reminders
                   (event_id, company_id, user_id, customer_id, deal_id, calendar_slug, reminder_kind, send_at, status)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending')
                 ON DUPLICATE KEY UPDATE company_id = VALUES(company_id), user_id = VALUES(user_id),
                   customer_id = VALUES(customer_id), deal_id = VALUES(deal_id),
                   calendar_slug = VALUES(calendar_slug), send_at = VALUES(send_at),
                   status = 'pending', sent_at = NULL, error_message = NULL",
            )
            .bind(appointment.event_id)
            .bind(appointment.company_id)
            .bind(user_id)
            .bind(appointment.customer_id)
            .bind(deal_id)
            .bind(appointment.slug)
            .bind(kind)
            .bind(mysql(send_at))
            .execute(pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE appointment_rep_reminders SET status = 'cancelled', error_message = NULL
                  WHERE event_id = ? AND reminder_kind = ? AND status = 'pending'",
            )
            .bind(appointment.event_id)
            .bind(kind)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Cancels every pending reminder of an event.
pub async fn cancel(pool: &MySqlPool, event_id: i32) -> Result<(), sqlx::Error> {
    for table in ["appointment_reminders", "appointment_rep_reminders"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table} SET status = 'cancelled', error_message = NULL
              WHERE event_id = ? AND status = 'pending'"
        )))
        .bind(event_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// After an event moves: re-times the reminders it already has.
///
/// An event gets them when it is booked for a customer. The rep is the one
/// already reminded, else the customer's sales rep, else whoever made it.
pub async fn resync_after_move(
    pool: &MySqlPool,
    event_id: i32,
    company_id: i32,
    created_user_id: i32,
    start: NaiveDateTime,
) -> Result<(), sqlx::Error> {
    let Some(slug) = eligible_slug(pool, event_id, company_id).await? else {
        return Ok(());
    };
    let customer_id: Option<i32> = sqlx::query_scalar(
        "SELECT CAST(customer_id AS SIGNED) FROM appointment_reminders WHERE event_id = ? ORDER BY id LIMIT 1",
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    let Some(customer_id) = customer_id else {
        return Ok(());
    };
    let now = Utc::now();
    let appointment = Appointment {
        event_id,
        company_id,
        customer_id,
        slug: &slug,
        start: start.and_utc(),
    };
    schedule_customer(pool, &appointment, now).await?;
    let context: Option<(i32, Option<i32>)> = sqlx::query_as(
        "SELECT CAST(user_id AS SIGNED), CAST(deal_id AS SIGNED) FROM appointment_rep_reminders WHERE event_id = ? ORDER BY id LIMIT 1",
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    let (user_id, deal_id) = if let Some((user_id, deal_id)) = context {
        (Some(user_id), deal_id)
    } else {
        let sales_rep: Option<Option<i32>> = sqlx::query_scalar(
            "SELECT CAST(sales_rep AS SIGNED) FROM customers WHERE id = ? AND company_id = ? AND deleted_at IS NULL LIMIT 1",
        )
        .bind(customer_id)
        .bind(company_id)
        .fetch_optional(pool)
        .await?;
        let rep = sales_rep
            .flatten()
            .filter(|id| *id > 0)
            .or_else(|| (created_user_id > 0).then_some(created_user_id));
        (rep, None)
    };
    if let Some(user_id) = user_id {
        schedule_rep(pool, &appointment, user_id, deal_id, now).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M")
            .unwrap()
            .and_utc()
    }

    #[test]
    fn customer_reminders_skip_past_times() {
        let start = at("2026-10-20 14:00");
        assert_eq!(
            customer_send_times(start, at("2026-10-12 09:00")),
            [
                ("24h", Some(at("2026-10-19 14:00"))),
                ("1h", Some(at("2026-10-20 13:00")))
            ]
        );
        assert_eq!(
            customer_send_times(start, at("2026-10-20 12:30")),
            [("24h", None), ("1h", Some(at("2026-10-20 13:00")))]
        );
    }

    #[test]
    fn the_rep_hears_at_5pm_store_time_the_day_before() {
        // 10:00 Indianapolis (EDT) on Oct 20 = 14:00 UTC; 5 PM EDT Oct 19 = 21:00 UTC.
        let start = at("2026-10-20 14:00");
        assert_eq!(
            rep_send_times(start, at("2026-10-12 09:00"))[0],
            ("day_before_5pm", Some(at("2026-10-19 21:00")))
        );
        assert_eq!(
            rep_send_times(start, at("2026-10-19 22:00"))[0],
            ("day_before_5pm", None)
        );
        // 1 AM UTC Oct 21 is still Oct 20 in Indianapolis.
        assert_eq!(
            rep_send_times(at("2026-10-21 01:00"), at("2026-10-12 09:00"))[0].1,
            Some(at("2026-10-19 21:00"))
        );
    }
}
