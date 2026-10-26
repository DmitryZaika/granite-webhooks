use super::queries::{all_calendars, ensure_system_calendars, visible_calendars};
use super::schemas::{Calendar, HiddenUser};
use crate::auth::{AdminUser, EmployeeUser};
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::openapi::IntoParams;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;

/// Query of `GET /v1/calendars`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Every live calendar, hidden ones too (admins only).
    #[serde(default)]
    pub all: bool,
}

/// List the calendars on the caller's schedule
///
/// The company's live calendars except those hidden for everyone or from
/// the caller in Admin > Schedule. The built-in Templates, Installations and
/// Estimates calendars come first and are created on the fly when a company
/// lacks them (so every company has them). With `all=true` (admins), every
/// live calendar, hidden ones too, for Admin > Schedule.
#[crate::openapi::path(
    get,
    path = "/v1/calendars",
    operation_id = "list_calendars",
    tag = "schedule",
    params(ListQuery),
    responses(
        (status = 200, description = "Calendars", body = Vec<Calendar>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin (`all`: not an admin)", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListQuery>,
) -> ApiResult<Json<Vec<Calendar>>> {
    ensure_system_calendars(&state.pool, user.company_id, user.id).await?;
    if query.all {
        if !user.is_admin && !user.is_superuser {
            return Err(ApiError::Forbidden);
        }
        return Ok(Json(all_calendars(&state.pool, user.company_id).await?));
    }
    Ok(Json(
        visible_calendars(&state.pool, user.company_id, user.id).await?,
    ))
}

/// Who each calendar is hidden from
///
/// Every (calendar, user) pair an admin hid in Admin > Schedule.
#[crate::openapi::path(
    get,
    path = "/v1/calendars/hidden-users",
    operation_id = "list_calendar_hidden_users",
    tag = "schedule",
    responses(
        (status = 200, description = "Pairs", body = Vec<HiddenUser>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn hidden_users(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
) -> ApiResult<Json<Vec<HiddenUser>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT calendar_id, user_id FROM calendar_hidden_users
              WHERE company_id = ? ORDER BY calendar_id, user_id",
        )
        .bind(user.company_id)
        .fetch_all(&state.pool)
        .await?,
    ))
}
