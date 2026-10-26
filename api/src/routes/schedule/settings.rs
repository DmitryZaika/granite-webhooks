//! The company's schedule settings (Admin > Schedule).
//!
//! - `GET /v1/schedule/settings` → the day the schedule's weeks start on and
//!   the calendar the Schedule estimate button books on
//! - `PUT /v1/schedule/settings` → an admin changes them

use crate::auth::{AdminUser, CurrentUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;
use utoipa::ToSchema;

/// The first day of a week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WeekStart {
    Sunday,
    Monday,
}

/// `GET /v1/schedule/settings`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleSettings {
    /// The day the week and month views start their weeks on.
    pub week_starts_on: WeekStart,
    /// The calendar the deal page's Schedule estimate button books on: the
    /// one an admin picked, else the built-in Estimates calendar (null when
    /// there is neither).
    pub estimates_calendar_id: Option<i32>,
}

/// `PUT /v1/schedule/settings`: only the fields sent change.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ScheduleSettingsInput {
    /// The day the week and month views start their weeks on.
    #[serde(default)]
    pub week_starts_on: Option<WeekStart>,
    /// A calendar of the caller's company.
    #[serde(default)]
    pub estimates_calendar_id: Option<i32>,
}

/// The calendar estimates are booked on: the one an admin picked while it
/// exists, else the built-in Estimates calendar.
pub async fn estimates_calendar_id(
    pool: &MySqlPool,
    company_id: i32,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT cal.id FROM calendars cal JOIN company co ON co.id = cal.company_id
          WHERE cal.company_id = ? AND cal.deleted_at IS NULL
            AND (cal.id = co.estimates_calendar_id OR cal.slug = 'estimates')
          ORDER BY COALESCE(cal.id = co.estimates_calendar_id, 0) DESC, cal.id
          LIMIT 1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await
}

/// Get the schedule settings
///
/// The day the company's schedule starts its weeks on (Sunday unless an
/// admin picked Monday) and the calendar estimates are booked on.
#[utoipa::path(
    get,
    path = "/v1/schedule/settings",
    operation_id = "get_schedule_settings",
    tag = "schedule",
    responses(
        (status = 200, description = "Settings", body = ScheduleSettings),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn settings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<ScheduleSettings>> {
    let monday: Option<bool> =
        sqlx::query_scalar("SELECT schedule_week_starts_monday <> 0 FROM company WHERE id = ?")
            .bind(user.company_id)
            .fetch_optional(&state.pool)
            .await?;
    Ok(Json(ScheduleSettings {
        week_starts_on: if monday.unwrap_or(false) {
            WeekStart::Monday
        } else {
            WeekStart::Sunday
        },
        estimates_calendar_id: estimates_calendar_id(&state.pool, user.company_id).await?,
    }))
}

/// Set the schedule settings
///
/// Saves the day the company's schedule starts its weeks on and the calendar
/// estimates are booked on, for everyone.
#[utoipa::path(
    put,
    path = "/v1/schedule/settings",
    operation_id = "set_schedule_settings",
    tag = "schedule",
    request_body = ScheduleSettingsInput,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not `sunday` or `monday`, or not a calendar of the company", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ScheduleSettingsInput>,
) -> ApiResult<StatusCode> {
    if let Some(calendar_id) = input.estimates_calendar_id {
        let found: Option<i32> = sqlx::query_scalar(
            "SELECT id FROM calendars WHERE id = ? AND company_id = ? AND deleted_at IS NULL",
        )
        .bind(calendar_id)
        .bind(user.company_id)
        .fetch_optional(&state.pool)
        .await?;
        if found.is_none() {
            return Err(ApiError::BadRequest("Calendar not found".into()));
        }
        sqlx::query("UPDATE company SET estimates_calendar_id = ? WHERE id = ?")
            .bind(calendar_id)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    if let Some(day) = input.week_starts_on {
        sqlx::query("UPDATE company SET schedule_week_starts_monday = ? WHERE id = ?")
            .bind(day == WeekStart::Monday)
            .bind(user.company_id)
            .execute(&state.pool)
            .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}
