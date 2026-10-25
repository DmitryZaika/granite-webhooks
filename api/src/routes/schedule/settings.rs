//! The company's schedule settings (Admin > Schedule).
//!
//! - `GET /v1/schedule/settings` → the day the schedule's weeks start on
//! - `PUT /v1/schedule/settings` → an admin changes it

use crate::auth::{AdminUser, CurrentUser};
use crate::error::{ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The first day of a week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WeekStart {
    Sunday,
    Monday,
}

/// `GET` and `PUT /v1/schedule/settings`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ScheduleSettings {
    /// The day the week and month views start their weeks on.
    pub week_starts_on: WeekStart,
}

/// Get the schedule settings
///
/// The day the company's schedule starts its weeks on (Sunday unless an
/// admin picked Monday).
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
    }))
}

/// Set the schedule settings
///
/// Saves the day the company's schedule starts its weeks on, for everyone.
#[utoipa::path(
    put,
    path = "/v1/schedule/settings",
    operation_id = "set_schedule_settings",
    tag = "schedule",
    request_body = ScheduleSettings,
    responses(
        (status = 204, description = "Saved"),
        (status = 400, description = "Not `sunday` or `monday`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn set_settings(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    JsonBody(input): JsonBody<ScheduleSettings>,
) -> ApiResult<StatusCode> {
    sqlx::query("UPDATE company SET schedule_week_starts_monday = ? WHERE id = ?")
        .bind(input.week_starts_on == WeekStart::Monday)
        .bind(user.company_id)
        .execute(&state.pool)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
