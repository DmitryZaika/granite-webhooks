//! The schedule's activity feed.
//!
//! - `GET /v1/schedule/activity` → the company's events by their latest change,
//!   each with its last few history lines and comments

use super::board::ScheduleEvent;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

/// Lines shown under each event.
const LINES_PER_EVENT: usize = 3;
/// Events per page when `limit` is left out.
const DEFAULT_LIMIT: i64 = 20;
/// Most events per page.
const MAX_LIMIT: i64 = 50;

/// Query of `GET /v1/schedule/activity`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ActivityQuery {
    /// At most this many events, 1 to 50 (default 20).
    pub limit: Option<i64>,
    /// Skip this many events (the next page).
    pub offset: Option<i64>,
}

/// A change to an event or a comment on it.
#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct ActivityLine {
    /// e.g. `created`, `date_updated`; `commented` for a comment.
    pub action: String,
    /// Shown text.
    pub message: String,
    /// Who.
    pub created_by: Option<String>,
    /// When.
    pub created_at: DateTime<Utc>,
}

/// An event with its latest activity.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActivityEvent {
    /// The event.
    pub event: ScheduleEvent,
    /// Waiting for its day (not on the calendar).
    pub unscheduled: bool,
    /// The installer or template maker on it.
    pub assignee_name: Option<String>,
    /// Who made it.
    pub creator_name: Option<String>,
    /// When it last changed.
    pub last_activity_at: DateTime<Utc>,
    /// Its last three lines, oldest first.
    pub lines: Vec<ActivityLine>,
}

/// `GET /v1/schedule/activity`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ScheduleActivity {
    /// Most recently changed first.
    pub events: Vec<ActivityEvent>,
    /// More events past this page.
    pub has_more: bool,
}

#[derive(sqlx::FromRow)]
struct ActivityRow {
    #[sqlx(flatten)]
    event: ScheduleEvent,
    unscheduled: bool,
    assignee_name: Option<String>,
    creator_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct LineRow {
    event_id: i32,
    #[sqlx(flatten)]
    line: ActivityLine,
}

// Comment logs are shown from the comments themselves (as on the event panel).
const HISTORY_LINES: &str = "SELECT h.event_id, h.action, h.message, h.created_by, h.created_at
   FROM event_history h
  WHERE h.company_id = ? AND h.created_at IS NOT NULL
    AND LOWER(TRIM(h.message)) NOT IN ('comment added', 'comment deleted', 'comment updated')";

const COMMENT_LINES: &str =
    "SELECT c.event_id, 'commented' AS action, 'Event commented' AS message,
        c.created_by, c.created_at
   FROM event_comments c
  WHERE c.company_id = ? AND c.deleted_at IS NULL AND c.created_at IS NOT NULL";

/// The schedule's activity
///
/// The company's events, most recently changed or commented first, each with
/// its last three history lines and comments. Deleted events and events of
/// calendars hidden from the caller are left out.
#[utoipa::path(
    get,
    path = "/v1/schedule/activity",
    operation_id = "list_schedule_activity",
    tag = "schedule",
    params(ActivityQuery),
    responses(
        (status = 200, description = "Activity", body = ScheduleActivity),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn activity(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ActivityQuery>,
) -> ApiResult<Json<ScheduleActivity>> {
    let pool = &state.pool;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    // Only constants and placeholders are spliced in.
    let latest: Vec<(i32, DateTime<Utc>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT a.event_id, MAX(a.created_at) AS last_at
           FROM ({HISTORY_LINES} UNION ALL {COMMENT_LINES}) a
           JOIN events e ON e.id = a.event_id AND e.deleted_date IS NULL
          WHERE e.calendar_id IS NULL OR NOT EXISTS (
                  SELECT 1 FROM calendar_hidden_users hu
                   WHERE hu.calendar_id = e.calendar_id AND hu.company_id = ?
                     AND hu.user_id = ?)
          GROUP BY a.event_id
          ORDER BY last_at DESC, a.event_id DESC
          LIMIT ? OFFSET ?"
    )))
    .bind(user.company_id)
    .bind(user.company_id)
    .bind(user.company_id)
    .bind(user.id)
    .bind(limit + 1)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let has_more = latest.len() > usize::try_from(limit).unwrap_or(0);
    let latest = &latest[..latest.len().min(usize::try_from(limit).unwrap_or(0))];
    if latest.is_empty() {
        return Ok(Json(ScheduleActivity {
            events: Vec::new(),
            has_more,
        }));
    }

    let mut sql = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT e.id, e.title, e.description, e.location,
                DATE_FORMAT(e.start_date, '%Y-%m-%dT%H:%i:%sZ') AS start_date,
                DATE_FORMAT(e.end_date, '%Y-%m-%dT%H:%i:%sZ') AS end_date,
                COALESCE(e.all_day, 0) <> 0 AS all_day, e.color, e.status, e.notes,
                e.calendar_id, NULLIF(TRIM(e.rrule), '') AS rrule,
                DATE_FORMAT(e.recurrence_until, '%Y-%m-%dT%H:%i:%sZ') AS recurrence_until,
                e.created_user_id, e.assigned_user_id, e.sale_id, e.customer_id,
                c.name AS customer_name, COALESCE(e.unscheduled, 0) <> 0 AS unscheduled,
                au.name AS assignee_name, u.name AS creator_name
           FROM events e
           JOIN users u ON e.created_user_id = u.id
           LEFT JOIN users au ON e.assigned_user_id = au.id
           LEFT JOIN customers c
             ON c.id = e.customer_id AND c.company_id = u.company_id AND c.deleted_at IS NULL
          WHERE u.company_id = ",
    );
    sql.push_bind(user.company_id).push(" AND e.id IN (");
    let mut ids = sql.separated(", ");
    for (id, _) in latest {
        ids.push_bind(*id);
    }
    sql.push(")");
    let rows: Vec<ActivityRow> = sql.build_query_as().fetch_all(pool).await?;
    let mut rows: HashMap<i32, ActivityRow> =
        rows.into_iter().map(|row| (row.event.id, row)).collect();

    let placeholders = vec!["?"; latest.len()].join(", ");
    let lines_sql = format!(
        "SELECT * FROM ({HISTORY_LINES} UNION ALL {COMMENT_LINES}) a
          WHERE a.event_id IN ({placeholders})
          ORDER BY a.created_at DESC"
    );
    let mut lines_query = sqlx::query_as::<_, LineRow>(sqlx::AssertSqlSafe(lines_sql))
        .bind(user.company_id)
        .bind(user.company_id);
    for (id, _) in latest {
        lines_query = lines_query.bind(*id);
    }
    let mut lines: HashMap<i32, Vec<ActivityLine>> = HashMap::new();
    for row in lines_query.fetch_all(pool).await? {
        let event_lines = lines.entry(row.event_id).or_default();
        if event_lines.len() < LINES_PER_EVENT {
            event_lines.push(row.line);
        }
    }

    let events = latest
        .iter()
        .filter_map(|(id, last_activity_at)| {
            let row = rows.remove(id)?;
            let mut event_lines = lines.remove(id).unwrap_or_default();
            event_lines.reverse();
            Some(ActivityEvent {
                event: row.event,
                unscheduled: row.unscheduled,
                assignee_name: row.assignee_name,
                creator_name: row.creator_name,
                last_activity_at: *last_activity_at,
                lines: event_lines,
            })
        })
        .collect();
    Ok(Json(ScheduleActivity { events, has_more }))
}
