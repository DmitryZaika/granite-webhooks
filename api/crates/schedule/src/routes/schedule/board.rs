//! The schedule page's data.
//!
//! - `GET /v1/schedule/events` → the events of a date range
//! - `GET /v1/schedule/unscheduled` → the events waiting for their day
//! - `GET /v1/schedule/people` → installers and template makers with their colors
//! - `PUT /v1/schedule/people/{user_id}/color` → an admin picks a person's color
//! - `GET /v1/me/schedule` → an installer's or template maker's own events
//!
//! Replaces the Remix `employee.schedule.$period` and `admin.schedule`
//! loaders and actions. Repeating events come back once, with their `rrule`:
//! the page expands them over the range.

use crate::auth::{AdminUser, CurrentUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::{JsonBody, MultiQuery};
use crate::openapi::{IntoParams, ToSchema};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// `positions.id` of an installer.
const INSTALLER: i32 = 6;
/// `positions.id` of a template maker.
const TEMPLATE: i32 = 11;

/// Colors a person can get (the schedule's `EVENT_COLORS`).
pub const COLORS: [&str; 12] = [
    "blue", "red", "green", "yellow", "purple", "pink", "orange", "teal", "cyan", "indigo", "lime",
    "slate",
];

/// Query of `GET /v1/schedule/events`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// Range start (ISO 8601).
    pub start: DateTime<Utc>,
    /// Range end (ISO 8601).
    pub end: DateTime<Utc>,
}

/// An event's QuickBooks invoice.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EventInvoice {
    /// QuickBooks invoice id.
    pub id: String,
    /// Its number.
    pub number: Option<String>,
}

/// An event on the schedule.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ScheduleEvent {
    /// Event id.
    pub id: i32,
    /// The event's name.
    pub title: String,
    /// Description.
    pub description: Option<String>,
    /// Address.
    pub location: Option<String>,
    /// Start (UTC, ISO 8601).
    pub start_date: String,
    /// End (UTC, ISO 8601).
    pub end_date: String,
    /// All day.
    pub all_day: bool,
    /// Label color.
    pub color: Option<String>,
    /// Status.
    pub status: Option<String>,
    /// Notes.
    pub notes: Option<String>,
    /// Calendar.
    pub calendar_id: Option<i32>,
    /// Repeat rule (RFC 5545), for repeating events.
    pub rrule: Option<String>,
    /// Last repeat (UTC, ISO 8601).
    pub recurrence_until: Option<String>,
    /// Days a repeating event skips: comma-separated `YYYY-MM-DD` in the
    /// shop's time zone. Null when none are skipped.
    #[sqlx(default)]
    pub recurrence_exdates: Option<String>,
    /// Who made it.
    pub created_user_id: i32,
    /// The installer or template maker on it.
    pub assigned_user_id: Option<i32>,
    /// Its sale.
    pub sale_id: Option<i32>,
    /// The customer it is for.
    pub customer_id: Option<i32>,
    /// That customer's name.
    pub customer_name: Option<String>,
    /// Its QuickBooks invoice (only when QuickBooks is connected).
    #[sqlx(skip)]
    pub qbo_invoice: Option<EventInvoice>,
}

/// `GET /v1/schedule/events`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleEvents {
    /// One-off events in the range and repeating events that may fall in it,
    /// by start; calendars hidden from the caller are left out.
    pub events: Vec<ScheduleEvent>,
    /// QuickBooks is on and connected: events show their invoices.
    pub quickbooks_connected: bool,
}

/// The schedule's events
///
/// One-off events overlapping the range and repeating events that started
/// before its end and repeat past its start (the page expands them), with
/// their customers' names and QuickBooks invoices. Events of calendars
/// hidden for everyone or from the caller are left out.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/events",
    operation_id = "list_schedule_events",
    tag = "schedule",
    params(EventsQuery),
    responses(
        (status = 200, description = "Events", body = ScheduleEvents),
        (status = 400, description = "Missing or invalid range", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn events(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<EventsQuery>,
) -> ApiResult<Json<ScheduleEvents>> {
    load_events(&state, user.company_id, &query, Scope::Board(user.id))
        .await
        .map(Json)
}

/// Which events of the company a range read returns.
enum Scope {
    /// The whole board, less the calendars hidden from this viewer.
    Board(i32),
    /// Only the events assigned to this person, whatever calendar they are on.
    AssignedTo(i32),
}

/// The company's events of a range (see [`events`]), with their invoices.
async fn load_events(
    state: &AppState,
    company_id: i32,
    query: &EventsQuery,
    scope: Scope,
) -> ApiResult<ScheduleEvents> {
    if query.end < query.start {
        return Err(ApiError::BadRequest(
            "The range ends before it starts".into(),
        ));
    }
    let (viewer, assigned) = match scope {
        Scope::Board(viewer) => (Some(viewer), None),
        Scope::AssignedTo(person) => (None, Some(person)),
    };
    let start = query.start.naive_utc();
    let end = query.end.naive_utc();
    let mut events: Vec<ScheduleEvent> = sqlx::query_as(
        "SELECT e.id, e.title, e.description, e.location,
                DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ') AS start_date,
                DATE_FORMAT(e.end_date, '%Y-%m-%dT%H:%i:%sZ') AS end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.color, e.status, e.notes,
                e.calendar_id, NULLIF(TRIM(e.rrule), '') AS rrule,
                DATE_FORMAT(e.recurrence_until, '%Y-%m-%dT%H:%i:%sZ') AS recurrence_until,
                NULLIF(TRIM(e.recurrence_exdates), '') AS recurrence_exdates,
                e.created_user_id, e.assigned_user_id, e.sale_id, e.customer_id,
                c.name AS customer_name
           FROM events e
           JOIN users u ON e.created_user_id = u.id
           LEFT JOIN customers c
             ON c.id = e.customer_id AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND e.deleted_date IS NULL AND e.unscheduled = 0
            AND (? IS NULL OR e.calendar_id IS NULL OR NOT EXISTS (
                  SELECT 1 FROM calendar_hidden_users h
                   WHERE h.calendar_id = e.calendar_id AND h.company_id = u.company_id
                     AND h.user_id = ?))
            AND (? IS NULL OR e.assigned_user_id = ?)
            AND (
              (NULLIF(TRIM(e.rrule), '') IS NULL
                AND e.start_date <= ? AND e.end_date >= ?)
              OR (NULLIF(TRIM(e.rrule), '') IS NOT NULL
                AND e.start_date <= ?
                AND (e.recurrence_until IS NULL OR e.recurrence_until >= ?))
            )
          ORDER BY e.start_date, e.id",
    )
    .bind(company_id)
    .bind(viewer)
    .bind(viewer)
    .bind(assigned)
    .bind(assigned)
    .bind(end)
    .bind(start)
    .bind(end)
    .bind(start)
    .fetch_all(&state.pool)
    .await?;
    let features =
        crate::routes::company::queries::company_features(&state.pool, company_id).await?;
    let quickbooks_connected = features.quickbooks_enabled && features.quickbooks_connected;
    if quickbooks_connected && !events.is_empty() {
        let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
            "SELECT event_id, qbo_invoice_id, qbo_invoice_number FROM event_qbo_invoices
              WHERE company_id = ",
        );
        sql.push_bind(company_id).push(" AND event_id IN (");
        let mut ids = sql.separated(", ");
        for event in &events {
            ids.push_bind(event.id);
        }
        sql.push(")");
        let rows: Vec<(i32, String, Option<String>)> =
            sql.build_query_as().fetch_all(&state.pool).await?;
        let invoices: HashMap<i32, EventInvoice> = rows
            .into_iter()
            .map(|(event, id, number)| (event, EventInvoice { id, number }))
            .collect();
        for event in &mut events {
            event.qbo_invoice = invoices.get(&event.id).cloned();
        }
    }
    Ok(ScheduleEvents {
        events,
        quickbooks_connected,
    })
}

/// Events waiting for their day
///
/// The company's unscheduled events (kept off the calendar until someone
/// picks the date), oldest first, with their customers' names. Events of
/// calendars hidden for the caller are left out.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/unscheduled",
    operation_id = "list_unscheduled_events",
    tag = "schedule",
    responses(
        (status = 200, description = "Events", body = Vec<ScheduleEvent>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn unscheduled(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<ScheduleEvent>>> {
    let events = sqlx::query_as(
        "SELECT e.id, e.title, e.description, e.location,
                DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ') AS start_date,
                DATE_FORMAT(e.end_date, '%Y-%m-%dT%H:%i:%sZ') AS end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.color, e.status, e.notes,
                e.calendar_id, NULL AS rrule, NULL AS recurrence_until,
                e.created_user_id, e.assigned_user_id, e.sale_id, e.customer_id,
                c.name AS customer_name
           FROM events e
           JOIN users u ON e.created_user_id = u.id
           LEFT JOIN customers c
             ON c.id = e.customer_id AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ? AND e.deleted_date IS NULL AND e.unscheduled = 1
            AND (e.calendar_id IS NULL OR NOT EXISTS (
                  SELECT 1 FROM calendar_hidden_users h
                   WHERE h.calendar_id = e.calendar_id AND h.company_id = u.company_id
                     AND h.user_id = ?))
          ORDER BY e.created_date, e.id",
    )
    .bind(user.company_id)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(events))
}

/// Query of `GET /v1/me/schedule`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MyScheduleQuery {
    /// Range start (ISO 8601).
    pub start: DateTime<Utc>,
    /// Range end (ISO 8601).
    pub end: DateTime<Utc>,
    /// Admins and superusers only: whose schedule, an installer or template
    /// maker of the company. The caller's own when left out.
    pub user_id: Option<i32>,
}

/// A personal schedule
///
/// The events assigned to the caller (an installer's or template maker's own
/// day), on any calendar, with their customers' names. An
/// admin or superuser may read an installer's or template maker's schedule
/// with `user_id`.
#[crate::openapi::path(
    get,
    path = "/v1/me/schedule",
    operation_id = "get_my_schedule",
    tag = "schedule",
    params(MyScheduleQuery),
    responses(
        (status = 200, description = "Events", body = ScheduleEvents),
        (status = 400, description = "Missing or invalid range, or not an installer or template maker of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Another person's schedule, and the caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn my_schedule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    MultiQuery(query): MultiQuery<MyScheduleQuery>,
) -> ApiResult<Json<ScheduleEvents>> {
    let person = query.user_id.unwrap_or(user.id);
    if person != user.id {
        if !(user.is_admin || user.is_superuser) {
            return Err(ApiError::Forbidden);
        }
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT u.id FROM users u JOIN users_positions up ON up.user_id = u.id
              WHERE u.id = ? AND up.company_id = ? AND up.position_id IN (?, ?)
                AND u.is_deleted = 0
              LIMIT 1",
        )
        .bind(person)
        .bind(user.company_id)
        .bind(INSTALLER)
        .bind(TEMPLATE)
        .fetch_optional(&state.pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest(
                "That person is not an installer or template for this company.".into(),
            ));
        }
    }
    let range = EventsQuery {
        start: query.start,
        end: query.end,
    };
    load_events(&state, user.company_id, &range, Scope::AssignedTo(person))
        .await
        .map(Json)
}

/// An installer or template maker.
#[derive(Debug, Serialize, ToSchema)]
pub struct SchedulePerson {
    /// User id.
    pub id: i32,
    /// Name ("Unnamed" when blank).
    pub name: String,
    /// `installer` or `template`.
    pub role: String,
    /// Their color on the schedule: the saved one, else one picked by id.
    pub color: String,
}

/// The color a person gets when none is saved.
fn default_color(user_id: i32) -> &'static str {
    let index = usize::try_from(user_id.unsigned_abs()).unwrap_or(0) % COLORS.len();
    COLORS[index]
}

/// The schedule's people
///
/// The company's installers and template makers (one entry per role a user
/// holds), by name, with their schedule colors.
#[crate::openapi::path(
    get,
    path = "/v1/schedule/people",
    operation_id = "list_schedule_people",
    tag = "schedule",
    responses(
        (status = 200, description = "People", body = Vec<SchedulePerson>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn people(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<SchedulePerson>>> {
    let rows: Vec<(i32, Option<String>, i32, Option<String>)> = sqlx::query_as(
        "SELECT u.id, u.name, up.position_id, sc.color
           FROM users u
           JOIN users_positions up ON up.user_id = u.id
           LEFT JOIN user_schedule_colors sc ON sc.user_id = u.id AND sc.company_id = up.company_id
          WHERE up.company_id = ? AND up.position_id IN (?, ?) AND u.is_deleted = 0
          ORDER BY u.name, u.id, up.position_id",
    )
    .bind(user.company_id)
    .bind(INSTALLER)
    .bind(TEMPLATE)
    .fetch_all(&state.pool)
    .await?;
    let mut seen = HashSet::new();
    let people = rows
        .into_iter()
        .filter(|(id, _, position, _)| seen.insert((*id, *position)))
        .map(|(id, name, position, saved)| SchedulePerson {
            id,
            name: name
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "Unnamed".into()),
            role: if position == INSTALLER {
                "installer"
            } else {
                "template"
            }
            .into(),
            color: saved
                .filter(|color| COLORS.contains(&color.as_str()))
                .unwrap_or_else(|| default_color(id).into()),
        })
        .collect();
    Ok(Json(people))
}

/// Body of `PUT /v1/schedule/people/{user_id}/color`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct PersonColor {
    /// One of the schedule colors (`blue`, `red`, ...).
    pub color: String,
}

/// Set a person's schedule color
///
/// Their events on the Installations or Templates calendar show in it.
#[crate::openapi::path(
    put,
    path = "/v1/schedule/people/{user_id}/color",
    operation_id = "set_schedule_person_color",
    tag = "schedule",
    params(("user_id" = i32, Path, description = "User id")),
    request_body = PersonColor,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not a schedule color, or not an installer or template maker of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_color(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(user_id): Path<i32>,
    JsonBody(input): JsonBody<PersonColor>,
) -> ApiResult<StatusCode> {
    if !COLORS.contains(&input.color.as_str()) {
        return Err(ApiError::BadRequest("Choose a valid color".into()));
    }
    let person: Option<i32> = sqlx::query_scalar(
        "SELECT u.id FROM users u JOIN users_positions up ON up.user_id = u.id
          WHERE u.id = ? AND up.company_id = ? AND up.position_id IN (?, ?) AND u.is_deleted = 0
          LIMIT 1",
    )
    .bind(user_id)
    .bind(user.company_id)
    .bind(INSTALLER)
    .bind(TEMPLATE)
    .fetch_optional(&state.pool)
    .await?;
    if person.is_none() {
        return Err(ApiError::BadRequest(
            "That person is not an installer or template for this company.".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO user_schedule_colors (company_id, user_id, color) VALUES (?, ?, ?)
         ON DUPLICATE KEY UPDATE color = VALUES(color)",
    )
    .bind(user.company_id)
    .bind(user_id)
    .bind(&input.color)
    .execute(&state.pool)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_colors_cycle_by_id() {
        assert_eq!(default_color(0), "blue");
        assert_eq!(default_color(11), "slate");
        assert_eq!(default_color(12), "blue");
        assert_eq!(default_color(-1), "red");
    }
}
