use super::queries::{ensure_system_calendars, visible_calendars};
use super::schemas::Calendar;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the calendars on the caller's schedule
///
/// The company's live calendars except those hidden for everyone or from
/// the caller in Admin > Schedule. The built-in Templates, Installations and
/// Estimates calendars come first and are created on the fly when a company
/// lacks them (so every company has them).
#[utoipa::path(
    get,
    path = "/v1/calendars",
    operation_id = "list_calendars",
    tag = "schedule",
    responses(
        (status = 200, description = "Calendars", body = Vec<Calendar>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<Calendar>>> {
    ensure_system_calendars(&state.pool, user.company_id, user.id).await?;
    Ok(Json(
        visible_calendars(&state.pool, user.company_id, user.id).await?,
    ))
}
