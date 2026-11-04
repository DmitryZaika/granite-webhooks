//! The rep's AI promises (`crate::promises`), for the CRM:
//!
//! - `GET   /v1/ai/promises` → the caller's open promises, quote tasks and
//!   approvals, each appointment checked against the schedule, and the
//!   customers whose text waits for their reply; with a customer, phone or
//!   email, only that customer's
//! - `PATCH /v1/ai/promises/{id}` → kept (`done`), not wanted (`dismissed`)
//!   or open again
//!
//! Only for users AI promises is on for (`users.ai_promises`).

use crate::auth::EmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::{IntoParams, ToSchema};
use crate::phone::company;
use crate::promises::waiting_customers;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Query of `GET /v1/ai/promises`. With more than one, a promise matching
/// any of them is the customer's.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PromisesQuery {
    /// Only this customer's promises.
    pub customer_id: Option<i32>,
    /// Only promises to this phone (its last 10 digits count).
    pub phone_digits: Option<String>,
    /// Only promises to this email.
    pub customer_email: Option<String>,
}

/// Something on the schedule at a time.
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct PromiseEvent {
    /// The event's name.
    pub title: String,
    /// When it starts.
    pub start: DateTime<Utc>,
}

/// A promise the rep made a customer, the quote work a customer gave them,
/// or something they sent a customer to approve.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AiPromise {
    /// Promise id.
    pub id: u64,
    /// `promise`: the rep's; `quote`: the customer's quote to make or update
    /// (one task per customer, what they asked for in `details`); `approval`:
    /// something the rep sent for the customer's approval (due a day later).
    pub kind: String,
    /// What the rep does in the CRM that closes the promise by itself:
    /// `contract` (a document sent to sign), `schedule` (an event for the
    /// customer on the schedule) or `call` (a call with them); null when only
    /// a message that keeps it, or Done, closes it. A quote task closes once
    /// one of the customer's quotes is saved or emailed.
    pub crm_action: Option<String>,
    /// Where it was said: `call`, `sms` or `email`.
    pub source: String,
    /// When it was said.
    pub promised_at: DateTime<Utc>,
    /// The customer; null for a caller whose number is on no customer.
    pub customer_id: Option<i32>,
    /// That customer's name.
    pub customer_name: Option<String>,
    /// The customer's phone (10 digits), for a call or a text.
    pub customer_digits: Option<String>,
    /// The customer's email, for an email.
    pub customer_email: Option<String>,
    /// The name the customer goes by in the call or message (for a caller
    /// whose number is on no customer).
    pub customer_label: Option<String>,
    /// A quote task's quote: the customer's newest.
    pub quote_id: Option<i32>,
    /// What to do, e.g. "Send the inventory link".
    pub title: String,
    /// The words that made it (for a quote task, its latest ask's).
    pub said: String,
    /// A quote task's asks, one per line: what the quote must have or change.
    pub details: Option<String>,
    /// When it was promised for.
    pub due_at: Option<DateTime<Utc>>,
    /// An appointment at a set time (a visit, measuring, a showroom meeting).
    pub is_appointment: bool,
    /// `sms` or `email` when a message keeps it.
    pub channel: Option<String>,
    /// That message, ready to send (null when the AI could not write it).
    pub draft: Option<String>,
    /// An appointment's event for the customer within 12 hours of it (null:
    /// not on the schedule, or not an appointment).
    #[sqlx(skip)]
    pub scheduled: Option<PromiseEvent>,
    /// Another appointment within the hour: one the rep promised someone
    /// else, or an event assigned to them.
    #[sqlx(skip)]
    pub clash: Option<PromiseEvent>,
}

/// A customer whose text waits for the caller's reply.
#[derive(Debug, Serialize, ToSchema)]
pub struct WaitingCustomer {
    /// 10 digits.
    pub phone_digits: String,
    /// The customer with that phone; null when it is on no customer.
    pub customer_id: Option<i32>,
    /// That customer's name.
    pub customer_name: Option<String>,
    /// Their last text.
    pub text: String,
    /// When they sent it.
    pub at: DateTime<Utc>,
}

/// Open promises.
#[derive(Debug, Serialize, ToSchema)]
pub struct AiPromises {
    /// Newest first.
    pub promises: Vec<AiPromise>,
    /// Oldest wait first; only without a customer, phone or email.
    pub waiting: Vec<WaitingCustomer>,
}

/// An hour either side of an appointment is the same time.
const CLASH_MINUTES: i64 = 60;

/// A repeating event assigned to the rep.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecurringEvent {
    pub title: String,
    /// The first occurrence (UTC).
    pub start: NaiveDateTime,
    pub finish: NaiveDateTime,
    pub recurrence_until: Option<NaiveDateTime>,
    /// Local dates left out, `YYYY-MM-DD` comma-separated.
    pub exdates: String,
    pub rrule: String,
}

fn weekday(code: &str) -> Option<Weekday> {
    let code = code.trim();
    Some(
        match code
            .get(code.len().checked_sub(2)?..)?
            .to_ascii_uppercase()
            .as_str()
        {
            "MO" => Weekday::Mon,
            "TU" => Weekday::Tue,
            "WE" => Weekday::Wed,
            "TH" => Weekday::Thu,
            "FR" => Weekday::Fri,
            "SA" => Weekday::Sat,
            "SU" => Weekday::Sun,
            _ => return None,
        },
    )
}

fn week_start(day: NaiveDate) -> NaiveDate {
    day - Duration::days(i64::from(day.weekday().num_days_from_monday()))
}

/// The start (UTC) of the event's occurrence that runs within the hour after
/// `at`, if one does: `FREQ=DAILY` or `WEEKLY` with `INTERVAL`, `BYDAY` and
/// `UNTIL`, as the schedule writes them, each at the first one's local time,
/// less the excluded local dates.
pub fn occurrence_over(event: &RecurringEvent, at: DateTime<Utc>, tz: Tz) -> Option<DateTime<Utc>> {
    let (mut freq, mut interval, mut by_day, mut until) = (None, 1_i64, Vec::new(), None);
    for part in event.rrule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(value.trim().to_ascii_uppercase()),
            "INTERVAL" => interval = value.trim().parse().unwrap_or(1).max(1),
            "BYDAY" => by_day = value.split(',').filter_map(weekday).collect(),
            "UNTIL" => {
                until = value
                    .trim()
                    .get(..8)
                    .and_then(|day| NaiveDate::parse_from_str(day, "%Y%m%d").ok());
            }
            _ => {}
        }
    }
    let ends = event
        .recurrence_until
        .map(|last| tz.from_utc_datetime(&last).date_naive());
    let until = match (until, ends) {
        (Some(rule), Some(column)) => Some(rule.min(column)),
        (rule, column) => rule.or(column),
    };
    let first = tz.from_utc_datetime(&event.start);
    let length = (event.finish - event.start).max(Duration::minutes(1));
    let skipped: Vec<&str> = event.exdates.split(',').map(str::trim).collect();
    let today = at.with_timezone(&tz).date_naive();
    // The one of the day before may still be running.
    for day in [today, today - Duration::days(1)] {
        let date = day.format("%Y-%m-%d").to_string();
        if day < first.date_naive()
            || until.is_some_and(|last| day > last)
            || skipped.contains(&date.as_str())
        {
            continue;
        }
        let on = match freq.as_deref() {
            Some("DAILY") => {
                (day - first.date_naive()).num_days() % interval == 0
                    && (by_day.is_empty() || by_day.contains(&day.weekday()))
            }
            Some("WEEKLY") => {
                (week_start(day) - week_start(first.date_naive())).num_days() / 7 % interval == 0
                    && if by_day.is_empty() {
                        day.weekday() == first.weekday()
                    } else {
                        by_day.contains(&day.weekday())
                    }
            }
            _ => false,
        };
        let Some(start) = on
            .then(|| {
                tz.from_local_datetime(&day.and_time(first.time()))
                    .earliest()
            })
            .flatten()
            .map(|start| start.with_timezone(&Utc))
        else {
            continue;
        };
        if start < at + Duration::hours(1) && start + length > at {
            return Some(start);
        }
    }
    None
}

/// Each appointment's event on the schedule, and what it clashes with.
async fn check_schedule(
    state: &AppState,
    user: &SessionUser,
    promises: &mut [AiPromise],
) -> sqlx::Result<()> {
    let user_id = user.id;
    if !promises
        .iter()
        .any(|p| p.is_appointment && p.due_at.is_some())
    {
        return Ok(());
    }
    let config = company::load(&state.pool, user.company_id)
        .await
        .unwrap_or(None);
    let tz = company::time_zone_of(config.as_ref());
    // Repeating events assigned to the rep, matched by their occurrences.
    let recurring: Vec<RecurringEvent> = sqlx::query_as(
        "SELECT COALESCE(title, '') AS title, start_date AS start,
                COALESCE(end_date, start_date + INTERVAL 1 HOUR) AS finish, recurrence_until,
                COALESCE(recurrence_exdates, '') AS exdates, rrule
           FROM events
          WHERE assigned_user_id = ? AND deleted_date IS NULL AND COALESCE(unscheduled, 0) = 0
            AND COALESCE(rrule, '') <> ''",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    let appointments: Vec<(u64, DateTime<Utc>, String)> = promises
        .iter()
        .filter(|p| p.kind == "promise" && p.is_appointment)
        .filter_map(|p| {
            let title = match p.customer_name.as_deref().map(str::trim) {
                Some(name) if !name.is_empty() => format!("{} — {name}", p.title),
                _ => p.title.clone(),
            };
            Some((p.id, p.due_at?, title))
        })
        .collect();
    for promise in promises.iter_mut() {
        let (true, Some(due)) = (
            promise.kind == "promise" && promise.is_appointment,
            promise.due_at,
        ) else {
            continue;
        };
        let at = due.naive_utc();
        if let Some(customer_id) = promise.customer_id {
            promise.scheduled = sqlx::query_as(
                "SELECT COALESCE(title, '') AS title, start_date AS start FROM events
                  WHERE customer_id = ? AND deleted_date IS NULL AND COALESCE(unscheduled, 0) = 0
                    AND start_date BETWEEN ? - INTERVAL 12 HOUR AND ? + INTERVAL 12 HOUR
                  ORDER BY ABS(TIMESTAMPDIFF(MINUTE, start_date, ?)) LIMIT 1",
            )
            .bind(customer_id)
            .bind(at)
            .bind(at)
            .bind(at)
            .fetch_optional(&state.pool)
            .await?;
        }
        promise.clash = appointments
            .iter()
            .find(|(id, other, _)| {
                *id != promise.id && (*other - due).num_minutes().abs() < CLASH_MINUTES
            })
            .map(|(_, other, title)| PromiseEvent {
                title: title.clone(),
                start: *other,
            });
        if promise.clash.is_none() {
            promise.clash = sqlx::query_as(
                "SELECT COALESCE(title, '') AS title, start_date AS start FROM events
                  WHERE assigned_user_id = ? AND deleted_date IS NULL AND COALESCE(unscheduled, 0) = 0
                    AND COALESCE(rrule, '') = ''
                    AND start_date < ? + INTERVAL 1 HOUR
                    AND COALESCE(end_date, start_date + INTERVAL 1 HOUR) > ?
                    AND (customer_id IS NULL OR customer_id <> COALESCE(?, 0))
                  ORDER BY start_date LIMIT 1",
            )
            .bind(user_id)
            .bind(at)
            .bind(at)
            .bind(promise.customer_id)
            .fetch_optional(&state.pool)
            .await?;
        }
        if promise.clash.is_none() {
            promise.clash = recurring.iter().find_map(|event| {
                occurrence_over(event, due, tz).map(|start| PromiseEvent {
                    title: event.title.clone(),
                    start,
                })
            });
        }
    }
    Ok(())
}

/// Body of `PATCH /v1/ai/promises/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PromiseUpdate {
    /// `done` (kept), `dismissed` (not a promise, or not wanted) or `open`.
    pub status: String,
}

async fn require_enabled(state: &AppState, user: &SessionUser) -> ApiResult<()> {
    let on: Option<i64> =
        sqlx::query_scalar("SELECT CAST(ai_promises AS SIGNED) FROM users WHERE id = ?")
            .bind(user.id)
            .fetch_optional(&state.pool)
            .await?;
    if on == Some(1) {
        Ok(())
    } else {
        Err(ApiError::Denied(
            "AI promises are not switched on for your account".into(),
        ))
    }
}

/// List the caller's open promises
///
/// What the caller promised customers on calls, texts and emails and has not
/// done yet, the quotes customers asked them to make or update (one task per
/// customer, with what they asked for), and what waits for a customer's
/// approval, newest first: each promise with the message that keeps it when
/// a text or email can, each appointment with its event on the schedule and
/// what it clashes with. With a customer, phone or email: only that
/// customer's; without, also the customers whose text waits for the
/// caller's reply.
#[crate::openapi::path(
    get,
    path = "/v1/ai/promises",
    operation_id = "list_ai_promises",
    tag = "ai",
    params(PromisesQuery),
    responses(
        (status = 200, description = "Open promises", body = AiPromises),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or AI promises are not on for them", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Query(query): Query<PromisesQuery>,
) -> ApiResult<Json<AiPromises>> {
    require_enabled(&state, &user).await?;
    let digits: String = query
        .phone_digits
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let digits = (digits.len() >= 10).then(|| digits[digits.len() - 10..].to_string());
    let email = query
        .customer_email
        .as_deref()
        .map(|email| email.trim().to_lowercase())
        .filter(|email| email.contains('@'));
    let customer_id = query.customer_id.filter(|id| *id > 0);
    let filtered = customer_id.is_some() || digits.is_some() || email.is_some();
    // A quote task opens the customer's newest quote (one made since counts).
    let mut promises: Vec<AiPromise> = sqlx::query_as(
        "SELECT p.id, CAST(p.kind AS CHAR) AS kind, CAST(p.crm_action AS CHAR) AS crm_action,
                CAST(p.source AS CHAR) AS source, p.promised_at, p.customer_id,
                c.name AS customer_name, p.customer_digits, p.customer_email, p.customer_label,
                COALESCE((SELECT q.id FROM quotes q
                           WHERE p.kind = 'quote' AND q.company_id = p.company_id
                             AND q.customer_id = p.customer_id AND q.deleted_at IS NULL
                           ORDER BY COALESCE(q.updated_at, q.created_date) DESC, q.id DESC
                           LIMIT 1),
                         p.quote_id) AS quote_id,
                p.title, p.said, p.details, p.due_at, p.is_appointment,
                CAST(p.channel AS CHAR) AS channel, p.draft
           FROM ai_promises p
           LEFT JOIN customers c ON c.id = p.customer_id AND c.deleted_at IS NULL
          WHERE p.user_id = ? AND p.company_id = ? AND p.status = 'open'
            AND (? = 0 OR p.customer_id = ? OR p.customer_digits = ? OR p.customer_email = ?)
          ORDER BY p.promised_at DESC, p.id DESC
          LIMIT 200",
    )
    .bind(user.id)
    .bind(user.company_id)
    .bind(filtered)
    .bind(customer_id)
    .bind(digits)
    .bind(email)
    .fetch_all(&state.pool)
    .await?;
    check_schedule(&state, &user, &mut promises).await?;
    let waiting = if filtered {
        Vec::new()
    } else {
        waiting_customers(&state.pool, user.company_id, user.id)
            .await?
            .into_iter()
            .map(|customer| WaitingCustomer {
                phone_digits: customer.phone_digits,
                customer_id: customer.customer_id,
                customer_name: customer.customer_name,
                text: customer.text,
                at: customer.at.and_utc(),
            })
            .collect()
    };
    Ok(Json(AiPromises { promises, waiting }))
}

/// Close or reopen a promise
///
/// `done` once the rep kept it (sending its message does this), `dismissed`
/// when it was not a promise or is not wanted, `open` to bring it back.
#[crate::openapi::path(
    patch,
    path = "/v1/ai/promises/{id}",
    operation_id = "update_ai_promise",
    tag = "ai",
    params(("id" = u64, Path, description = "Promise id")),
    request_body = PromiseUpdate,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Status is not done, dismissed or open", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, or AI promises are not on for them", body = ErrorBody),
        (status = 404, description = "Not one of the caller's promises", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<u64>,
    JsonBody(input): JsonBody<PromiseUpdate>,
) -> ApiResult<StatusCode> {
    require_enabled(&state, &user).await?;
    let status = input.status.trim();
    if !matches!(status, "done" | "dismissed" | "open") {
        return Err(ApiError::BadRequest(
            "Status must be done, dismissed or open".into(),
        ));
    }
    let found: Option<u64> = sqlx::query_scalar(
        "SELECT id FROM ai_promises WHERE id = ? AND user_id = ? AND company_id = ?",
    )
    .bind(id)
    .bind(user.id)
    .bind(user.company_id)
    .fetch_optional(&state.pool)
    .await?;
    found.ok_or(ApiError::NotFound)?;
    sqlx::query(
        "UPDATE ai_promises
            SET status = ?, closed_at = IF(? = 'open', NULL, UTC_TIMESTAMP())
          WHERE id = ?",
    )
    .bind(status)
    .bind(status)
    .bind(id)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
    }

    fn event(rrule: &str, start: &str, finish: &str, exdates: &str) -> RecurringEvent {
        RecurringEvent {
            title: "Team meeting".into(),
            start: at(start),
            finish: at(finish),
            recurrence_until: None,
            exdates: exdates.into(),
            rrule: rrule.into(),
        }
    }

    #[test]
    fn repeating_events_are_matched_by_their_occurrences() {
        let tz = chrono_tz::America::Indiana::Indianapolis;
        let utc = |text| at(text).and_utc();
        // Every other Friday, 10:00 to 11:00 local, from Oct 2.
        let meeting = event(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR",
            "2026-10-02 14:00",
            "2026-10-02 15:00",
            "",
        );
        assert_eq!(
            occurrence_over(&meeting, utc("2026-10-16 14:30"), tz),
            Some(utc("2026-10-16 14:00"))
        );
        // Within the hour before it starts.
        assert_eq!(
            occurrence_over(&meeting, utc("2026-10-16 13:30"), tz),
            Some(utc("2026-10-16 14:00"))
        );
        // The week off.
        assert_eq!(occurrence_over(&meeting, utc("2026-10-09 14:30"), tz), None);
        // Still 10:00 local once the clocks go back (15:00 UTC).
        assert_eq!(
            occurrence_over(&meeting, utc("2026-11-13 15:30"), tz),
            Some(utc("2026-11-13 15:00"))
        );
        // A date left out, and the end of the rule.
        let skipped = event(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR",
            "2026-10-02 14:00",
            "2026-10-02 15:00",
            "2026-10-16",
        );
        assert_eq!(occurrence_over(&skipped, utc("2026-10-16 14:30"), tz), None);
        let ended = event(
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=FR;UNTIL=20261010",
            "2026-10-02 14:00",
            "2026-10-02 15:00",
            "",
        );
        assert_eq!(occurrence_over(&ended, utc("2026-10-16 14:30"), tz), None);
        // Every day, all day.
        let daily = event("FREQ=DAILY", "2026-10-01 04:00", "2026-10-02 03:59", "");
        assert_eq!(
            occurrence_over(&daily, utc("2026-10-20 18:00"), tz),
            Some(utc("2026-10-20 04:00"))
        );
    }
}
