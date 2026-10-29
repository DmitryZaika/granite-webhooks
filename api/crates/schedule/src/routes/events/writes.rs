//! Create, edit and delete schedule events. Remix: `api.events.ts`.

use super::history::{History, record};
use super::reminders;
use crate::auth::ShopOrEmployeeUser;
use crate::auth::session::SessionUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::openapi::ToSchema;
use crate::routes::schedule::{board, notifications};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

/// The QuickBooks invoice shown on an event.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventInvoice {
    /// QuickBooks invoice id (digits); empty or null unlinks the invoice.
    #[serde(default)]
    pub id: Option<String>,
    /// Its number as shown in QuickBooks.
    #[serde(default)]
    pub number: Option<String>,
}

/// Body of `POST /v1/events` and `PUT /v1/events/{id}`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EventInput {
    /// 1 to 255 characters.
    pub title: String,
    /// Longer text.
    #[serde(default)]
    pub description: Option<String>,
    /// Internal notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Address, up to 500 characters.
    #[serde(default)]
    pub location: Option<String>,
    /// Label color (default `blue`).
    #[serde(default)]
    pub color: Option<String>,
    /// Default `scheduled`.
    #[serde(default)]
    pub status: Option<String>,
    /// All-day event.
    #[serde(default)]
    pub all_day: bool,
    /// Waiting for its day: off the calendar, in the unscheduled list. The
    /// dates may then be left empty (they become now).
    #[serde(default)]
    pub unscheduled: bool,
    /// The company calendar it is on; required.
    #[serde(default)]
    pub calendar_id: Option<i32>,
    /// iCalendar repeat rule, up to 500 characters.
    #[serde(default)]
    pub rrule: Option<String>,
    /// Last repeat, UTC `YYYY-MM-DD HH:MM:SS` (only with `rrule`).
    #[serde(default)]
    pub recurrence_until: Option<String>,
    /// Start, UTC `YYYY-MM-DD HH:MM:SS`.
    pub start_date: String,
    /// End, UTC `YYYY-MM-DD HH:MM:SS`; not before the start.
    pub end_date: String,
    /// The user it is assigned to.
    #[serde(default)]
    pub assigned_user_id: Option<i32>,
    /// The sale it is for.
    #[serde(default)]
    pub sale_id: Option<i32>,
    /// The customer it is for, one of the company's; null unlinks them,
    /// leave out to keep them. The title stays the event's own name.
    #[serde(default, deserialize_with = "crate::serde_helpers::double_option")]
    #[schema(value_type = Option<i32>)]
    pub customer_id: Option<Option<i32>>,
    /// Phone for this event only (a spouse, a homeowner on a contractor's
    /// account), up to 50 characters; empty or null uses the customer's,
    /// leave out to keep it. Never changes the customer.
    #[serde(default, deserialize_with = "crate::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    pub contact_phone: Option<Option<String>>,
    /// Email for this event only, up to 255 characters; like `contact_phone`.
    #[serde(default, deserialize_with = "crate::serde_helpers::double_option")]
    #[schema(value_type = Option<String>)]
    pub contact_email: Option<Option<String>>,
    /// Link or unlink the QuickBooks invoice; leave out to keep it.
    #[serde(default)]
    pub qbo_invoice: Option<EventInvoice>,
    /// Create only: copy comments, comment files and these attachments
    /// from this event of the company.
    #[serde(default)]
    pub copy_from_event_id: Option<i32>,
    /// Create only: attachments of `copy_from_event_id` to copy.
    #[serde(default)]
    pub copy_attachment_ids: Vec<i32>,
    /// Its calendar's default notification flow, turned off for this
    /// event; leave out to keep the saved choice.
    #[serde(default)]
    pub skipped_notification_flow_ids: Option<Vec<i32>>,
    /// Its calendar's other notification flows picked for this event;
    /// leave out to keep the saved choice.
    #[serde(default)]
    pub picked_notification_flow_ids: Option<Vec<i32>>,
}

/// `POST /v1/events`.
#[derive(Debug, Serialize, ToSchema)]
pub struct CreatedEvent {
    /// The new event.
    pub id: u64,
}

/// Leading `YYYY-MM-DD` with an optional `[T ]HH:MM[:SS]`; the rest (a zone
/// suffix, fractions) is ignored, as the Remix route did.
pub fn parse_event_time(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim();
    let date = NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()?;
    let rest = text.get(10..).unwrap_or("");
    let Some(time) = rest.strip_prefix(['T', ' ']) else {
        return date.and_hms_opt(0, 0, 0);
    };
    let hour = time.get(..2)?.parse().ok()?;
    let minute = time.get(3..5)?.parse().ok()?;
    let second = match time.get(5..6) {
        Some(":") => time.get(6..8)?.parse().ok()?,
        _ => 0,
    };
    date.and_hms_opt(hour, minute, second)
}

fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|value| !value.is_empty())
}

/// The input, checked, with defaults applied.
pub struct Checked<'a> {
    pub title: &'a str,
    pub description: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub location: Option<&'a str>,
    pub color: &'a str,
    pub status: &'a str,
    pub calendar_id: i32,
    pub rrule: Option<&'a str>,
    pub recurrence_until: Option<NaiveDateTime>,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    /// `None` keeps the saved value; `Some(None)` clears it.
    pub contact_phone: Option<Option<&'a str>>,
    pub contact_email: Option<Option<&'a str>>,
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

pub fn check<'a>(input: &'a EventInput) -> ApiResult<Checked<'a>> {
    let length = |text: &str| text.chars().count();
    if input.title.is_empty() {
        return Err(bad("Title is required"));
    }
    if length(&input.title) > 255 {
        return Err(bad("Title must be 255 characters or less"));
    }
    let color = non_empty(input.color.as_ref()).unwrap_or("blue");
    if length(color) > 50 {
        return Err(bad("Color must be 50 characters or less"));
    }
    let status = non_empty(input.status.as_ref()).unwrap_or("scheduled");
    if length(status) > 50 {
        return Err(bad("Status must be 50 characters or less"));
    }
    let location = non_empty(input.location.as_ref());
    if location.is_some_and(|location| length(location) > 500) {
        return Err(bad("Location must be 500 characters or less"));
    }
    let contact = |value: &'a Option<Option<String>>| {
        value.as_ref().map(|value| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    };
    let contact_phone = contact(&input.contact_phone);
    if contact_phone
        .flatten()
        .is_some_and(|phone| length(phone) > 50)
    {
        return Err(bad("Phone must be 50 characters or less"));
    }
    let contact_email = contact(&input.contact_email);
    if contact_email
        .flatten()
        .is_some_and(|email| length(email) > 255)
    {
        return Err(bad("Email must be 255 characters or less"));
    }
    let calendar_id = input
        .calendar_id
        .filter(|id| *id > 0)
        .ok_or_else(|| bad("Calendar is required"))?;
    let rrule = input
        .rrule
        .as_deref()
        .map(str::trim)
        .filter(|rule| !rule.is_empty() && !input.unscheduled);
    if rrule.is_some_and(|rule| length(rule) > 500) {
        return Err(bad("Recurrence rule must be 500 characters or less"));
    }
    let time = |text: &str, message: &str| match parse_event_time(text) {
        Some(at) => Ok(at),
        None if input.unscheduled && text.trim().is_empty() => Ok(Utc::now().naive_utc()),
        None => Err(bad(message)),
    };
    let start = time(&input.start_date, "Invalid start date")?;
    let end = time(&input.end_date, "Invalid end date")?;
    if end < start {
        return Err(bad("End date must be on or after start date"));
    }
    let until_text = rrule.and_then(|_| non_empty(input.recurrence_until.as_ref()));
    let recurrence_until = match until_text {
        Some(text) => Some(parse_event_time(text).ok_or_else(|| bad("Invalid recurrence end"))?),
        None => None,
    };
    if recurrence_until.is_some_and(|until| until < start) {
        return Err(bad("Recurrence end must be on or after the start date"));
    }
    Ok(Checked {
        title: &input.title,
        description: non_empty(input.description.as_ref()),
        notes: non_empty(input.notes.as_ref()),
        location,
        color,
        status,
        calendar_id,
        rrule,
        recurrence_until,
        start,
        end,
        contact_phone,
        contact_email,
    })
}

async fn check_calendar(pool: &MySqlPool, company_id: i32, calendar_id: i32) -> ApiResult<()> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
    )
    .bind(calendar_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    found.map(|_| ()).ok_or_else(|| bad("Calendar not found"))
}

/// The customer the input links (0 is none), `None` when it leaves the link
/// as it is; a deleted customer of the company still counts, so their
/// events can be saved.
async fn check_customer(
    pool: &MySqlPool,
    company_id: i32,
    input: &EventInput,
) -> ApiResult<Option<Option<i32>>> {
    let Some(customer) = input.customer_id.map(|id| id.filter(|id| *id > 0)) else {
        return Ok(None);
    };
    if let Some(id) = customer {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT id FROM customers WHERE id = ? AND company_id = ?")
                .bind(id)
                .bind(company_id)
                .fetch_optional(pool)
                .await?;
        found.ok_or_else(|| bad("Customer not found"))?;
    }
    Ok(Some(customer))
}

/// Links or unlinks the event's QuickBooks invoice.
async fn save_invoice(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
    user_id: Option<i32>,
    invoice: &EventInvoice,
) -> Result<(), sqlx::Error> {
    let id = invoice.id.as_deref().map_or("", str::trim);
    if id.is_empty() {
        sqlx::query("DELETE FROM event_qbo_invoices WHERE event_id = ? AND company_id = ?")
            .bind(event_id)
            .bind(company_id)
            .execute(pool)
            .await?;
        return Ok(());
    }
    if id.len() > 32 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(());
    }
    let number = invoice
        .number
        .as_deref()
        .map(str::trim)
        .filter(|number| !number.is_empty())
        .map(|number| number.chars().take(64).collect::<String>());
    sqlx::query(
        "INSERT INTO event_qbo_invoices (event_id, company_id, qbo_invoice_id, qbo_invoice_number, created_by)
         VALUES (?, ?, ?, ?, ?)
         ON DUPLICATE KEY UPDATE qbo_invoice_id = VALUES(qbo_invoice_id),
           qbo_invoice_number = VALUES(qbo_invoice_number)",
    )
    .bind(event_id)
    .bind(company_id)
    .bind(id)
    .bind(number)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// `N thing` / `N things`.
fn counted(count: usize, thing: &str) -> String {
    if count == 1 {
        format!("1 {thing}")
    } else {
        format!("{count} {thing}s")
    }
}

/// Copies comments, comment files and the chosen attachments of another
/// event of the company onto a new one.
async fn copy_records(
    pool: &MySqlPool,
    user: &SessionUser,
    event_id: i32,
    source_id: i32,
    attachment_ids: &[i32],
) -> Result<(), sqlx::Error> {
    if company_event(pool, user.company_id, source_id)
        .await?
        .is_none()
    {
        return Ok(());
    }
    let company_id = user.company_id;
    let by = user.name.as_deref();
    let mut copied = 0;
    let mut ids = attachment_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    for attachment_id in ids {
        copied += sqlx::query(
            "INSERT INTO event_attachments (event_id, company_id, url, name, kind, created_by)
             SELECT ?, company_id, url, name, kind, ? FROM event_attachments
              WHERE id = ? AND event_id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(event_id)
        .bind(by)
        .bind(attachment_id)
        .bind(source_id)
        .bind(company_id)
        .execute(pool)
        .await?
        .rows_affected();
    }
    if copied > 0 {
        let message = format!(
            "Copied {}",
            counted(usize::try_from(copied).unwrap_or(0), "attachment")
        );
        record(
            pool,
            &History::new(event_id, company_id, "attachment_added", by).message(&message),
        )
        .await;
    }
    let comments = sqlx::query(
        "INSERT INTO event_comments (event_id, company_id, content, created_by)
         SELECT ?, company_id, content, created_by FROM event_comments
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(event_id)
    .bind(source_id)
    .bind(company_id)
    .execute(pool)
    .await?
    .rows_affected();
    if comments > 0 {
        let message = format!(
            "Copied {}",
            counted(usize::try_from(comments).unwrap_or(0), "comment")
        );
        record(
            pool,
            &History::new(event_id, company_id, "comment_added", by).message(&message),
        )
        .await;
    }
    sqlx::query(
        "INSERT INTO event_images (event_id, company_id, url, created_by)
         SELECT ?, company_id, url, created_by FROM event_images
          WHERE event_id = ? AND company_id = ? AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(event_id)
    .bind(source_id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A live event of the company (made by one of its users).
#[derive(sqlx::FromRow)]
pub struct StoredEvent {
    pub title: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_date: NaiveDateTime,
    pub end_date: NaiveDateTime,
    pub all_day: bool,
    pub unscheduled: bool,
    pub color: Option<String>,
    pub status: Option<String>,
    pub notes: Option<String>,
    pub calendar_id: Option<i32>,
    pub rrule: Option<String>,
    pub recurrence_until: Option<NaiveDateTime>,
    pub created_user_id: i32,
    pub customer_id: Option<i32>,
}

pub async fn company_event(
    pool: &MySqlPool,
    company_id: i32,
    event_id: i32,
) -> Result<Option<StoredEvent>, sqlx::Error> {
    sqlx::query_as(
        "SELECT e.title, e.description, e.location, e.start_date, e.end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.unscheduled <> 0 AS unscheduled,
                e.color, e.status, e.notes, e.calendar_id, e.rrule, e.recurrence_until,
                e.created_user_id, e.customer_id
           FROM events e JOIN users u ON u.id = e.created_user_id
          WHERE e.id = ? AND u.company_id = ? AND e.deleted_date IS NULL",
    )
    .bind(event_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

const fn not_found() -> ApiError {
    ApiError::NotFound
}

/// Add an event
///
/// Puts an event on one of the company's calendars, optionally copying
/// comments and attachments from another event.
#[crate::openapi::path(
    post,
    path = "/v1/events",
    operation_id = "create_event",
    tag = "schedule",
    request_body = EventInput,
    responses(
        (status = 201, description = "Added", body = CreatedEvent),
        (status = 400, description = "Invalid field, or unknown calendar or customer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn create(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    JsonBody(input): JsonBody<EventInput>,
) -> ApiResult<(StatusCode, Json<CreatedEvent>)> {
    let pool = &state.pool;
    let event = check(&input)?;
    check_calendar(pool, user.company_id, event.calendar_id).await?;
    let customer = check_customer(pool, user.company_id, &input).await?;
    let assignee = match input.assigned_user_id.filter(|id| *id > 0) {
        Some(id) => Some(id),
        None => board::sole_template_maker(pool, event.calendar_id).await?,
    };
    let id = sqlx::query(
        "INSERT INTO events (title, description, location, start_date, end_date, all_day,
            unscheduled, color, status, notes, created_user_id, assigned_user_id, sale_id,
            customer_id, calendar_id, rrule, recurrence_until, contact_phone, contact_email)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(event.title)
    .bind(event.description)
    .bind(event.location)
    .bind(event.start)
    .bind(event.end)
    .bind(input.all_day)
    .bind(input.unscheduled)
    .bind(event.color)
    .bind(event.status)
    .bind(event.notes)
    .bind(user.id)
    .bind(assignee)
    .bind(input.sale_id.filter(|id| *id > 0))
    .bind(customer.flatten())
    .bind(event.calendar_id)
    .bind(event.rrule)
    .bind(event.recurrence_until)
    .bind(event.contact_phone.flatten())
    .bind(event.contact_email.flatten())
    .execute(pool)
    .await?
    .last_insert_id();
    let event_id = i32::try_from(id).map_err(|_| ApiError::Internal("event id".into()))?;
    let by = user.name.as_deref();
    record(
        pool,
        &History::new(event_id, user.company_id, "created", by),
    )
    .await;
    if let Some(source) = input.copy_from_event_id.filter(|id| *id > 0) {
        copy_records(pool, &user, event_id, source, &input.copy_attachment_ids).await?;
    }
    if let Some(invoice) = &input.qbo_invoice {
        save_invoice(pool, user.company_id, event_id, Some(user.id), invoice).await?;
    }
    if let Some(skipped) = &input.skipped_notification_flow_ids {
        notifications::save_event_skips(pool, user.company_id, event_id, skipped).await?;
    }
    if let Some(picked) = &input.picked_notification_flow_ids {
        notifications::save_event_picks(pool, user.company_id, event_id, picked).await?;
    }
    Ok((StatusCode::CREATED, Json(CreatedEvent { id })))
}

/// History lines of an edit, besides date, label and location.
fn change_messages(
    event: &Checked<'_>,
    unscheduled: bool,
    customer: Option<Option<i32>>,
    existing: &StoredEvent,
) -> Vec<String> {
    let mut messages = Vec::new();
    if unscheduled != existing.unscheduled {
        let message = if unscheduled {
            "Moved to unscheduled"
        } else {
            "Scheduled"
        };
        messages.push(message.into());
    }
    if event.title != existing.title {
        messages.push(format!("Title changed to \"{}\"", event.title));
    }
    if let Some(customer) = customer.filter(|customer| *customer != existing.customer_id) {
        let message = if customer.is_some() {
            "Customer changed"
        } else {
            "Customer removed"
        };
        messages.push(message.into());
    }
    if event.description != existing.description.as_deref().filter(|d| !d.is_empty()) {
        messages.push("Description updated".into());
    }
    if event.notes != existing.notes.as_deref().filter(|n| !n.is_empty()) {
        messages.push("Notes updated".into());
    }
    if Some(event.status) != existing.status.as_deref() {
        messages.push(format!("Status changed to {}", event.status));
    }
    if Some(event.calendar_id) != existing.calendar_id {
        messages.push("Calendar changed".into());
    }
    if event.rrule != existing.rrule.as_deref().filter(|r| !r.is_empty())
        || event.recurrence_until != existing.recurrence_until
    {
        messages.push("Repeat updated".into());
    }
    messages
}

/// Edit an event
///
/// Saves every field, logs what changed in the event's history, and
/// re-times its appointment reminders when it moved.
#[crate::openapi::path(
    put,
    path = "/v1/events/{id}",
    operation_id = "update_event",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    request_body = EventInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Invalid field, or unknown calendar or customer", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn update(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
    JsonBody(input): JsonBody<EventInput>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    let existing = company_event(pool, user.company_id, id)
        .await?
        .ok_or_else(not_found)?;
    let event = check(&input)?;
    check_calendar(pool, user.company_id, event.calendar_id).await?;
    let customer = check_customer(pool, user.company_id, &input).await?;
    sqlx::query(
        "UPDATE events SET title = ?, description = ?, location = ?, start_date = ?, end_date = ?,
            all_day = ?, unscheduled = ?, color = ?, status = ?, notes = ?, assigned_user_id = ?,
            sale_id = ?, customer_id = IF(?, ?, customer_id), calendar_id = ?, rrule = ?,
            recurrence_until = ?, contact_phone = IF(?, ?, contact_phone),
            contact_email = IF(?, ?, contact_email), updated_date = CURRENT_TIMESTAMP
          WHERE id = ?",
    )
    .bind(event.title)
    .bind(event.description)
    .bind(event.location)
    .bind(event.start)
    .bind(event.end)
    .bind(input.all_day)
    .bind(input.unscheduled)
    .bind(event.color)
    .bind(event.status)
    .bind(event.notes)
    .bind(input.assigned_user_id.filter(|id| *id > 0))
    .bind(input.sale_id.filter(|id| *id > 0))
    .bind(customer.is_some())
    .bind(customer.flatten())
    .bind(event.calendar_id)
    .bind(event.rrule)
    .bind(event.recurrence_until)
    .bind(event.contact_phone.is_some())
    .bind(event.contact_phone.flatten())
    .bind(event.contact_email.is_some())
    .bind(event.contact_email.flatten())
    .bind(id)
    .execute(pool)
    .await?;
    if let Some(invoice) = &input.qbo_invoice {
        save_invoice(pool, user.company_id, id, None, invoice).await?;
    }
    if let Some(skipped) = &input.skipped_notification_flow_ids {
        notifications::save_event_skips(pool, user.company_id, id, skipped).await?;
    }
    if let Some(picked) = &input.picked_notification_flow_ids {
        notifications::save_event_picks(pool, user.company_id, id, picked).await?;
    }
    let by = user.name.as_deref();
    let moved = event.start != existing.start_date
        || event.end != existing.end_date
        || input.all_day != existing.all_day
        || input.unscheduled != existing.unscheduled;
    let mut actions = Vec::new();
    if moved {
        actions.push("date_updated");
    }
    if Some(event.color) != existing.color.as_deref() {
        actions.push("label_updated");
    }
    if event.location != existing.location.as_deref().filter(|l| !l.is_empty()) {
        actions.push("location_updated");
    }
    for action in actions {
        record(pool, &History::new(id, user.company_id, action, by)).await;
    }
    for message in change_messages(&event, input.unscheduled, customer, &existing) {
        record(
            pool,
            &History::new(id, user.company_id, "updated", by).message(&message),
        )
        .await;
    }
    // The crew's On my way, Arrived and Completed belong to the job's day.
    if input.unscheduled
        || super::crew::job_days(event.start, event.end, input.all_day).0
            != super::crew::job_days(existing.start_date, existing.end_date, existing.all_day).0
    {
        super::crew::clear(pool, id).await?;
    }
    if input.unscheduled {
        reminders::cancel(pool, id).await?;
    } else if moved {
        reminders::resync_after_move(
            pool,
            id,
            user.company_id,
            existing.created_user_id,
            event.start,
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete an event
///
/// Hides the event and cancels its pending appointment reminders.
#[crate::openapi::path(
    delete,
    path = "/v1/events/{id}",
    operation_id = "delete_event",
    tag = "schedule",
    params(("id" = i32, Path, description = "Event id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee, admin or shop manager or worker", body = ErrorBody),
        (status = 404, description = "No such event in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn delete(
    State(state): State<AppState>,
    ShopOrEmployeeUser(user): ShopOrEmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<StatusCode> {
    let pool = &state.pool;
    company_event(pool, user.company_id, id)
        .await?
        .ok_or_else(not_found)?;
    sqlx::query("UPDATE events SET deleted_date = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    reminders::cancel(pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::parse_event_time;

    #[test]
    fn event_times_read_like_the_remix_route() {
        let parse = |text| parse_event_time(text).map(|at| at.to_string());
        assert_eq!(
            parse("2026-10-20 14:30:05").as_deref(),
            Some("2026-10-20 14:30:05")
        );
        assert_eq!(
            parse("2026-10-20T14:30").as_deref(),
            Some("2026-10-20 14:30:00")
        );
        assert_eq!(
            parse("2026-10-20T14:30:00.000Z").as_deref(),
            Some("2026-10-20 14:30:00")
        );
        assert_eq!(parse("2026-10-20").as_deref(), Some("2026-10-20 00:00:00"));
        assert_eq!(parse("tomorrow"), None);
    }
}
