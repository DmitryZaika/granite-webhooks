use super::queries::{record_event, replace_layout, saved_layout};
use super::schemas::{DashboardLayout, WidgetEvent};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::JsonBody;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

/// Get my dashboard layout
///
/// Returns the widgets the signed-in user keeps on their employee dashboard in
/// the company the request acts in, in screen order. An empty list means no
/// layout is saved (or every widget was removed). Save it with
/// `PUT /v1/me/dashboard/widgets`.
#[utoipa::path(
    get,
    path = "/v1/me/dashboard/widgets",
    operation_id = "get_my_dashboard_layout",
    tag = "dashboard",
    responses(
        (status = 200, description = "The saved layout", body = DashboardLayout),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn layout(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<DashboardLayout>> {
    Ok(Json(DashboardLayout {
        widget_ids: saved_layout(&state.pool, user.id, user.company_id).await?,
    }))
}

/// Save my dashboard layout
///
/// Replaces the signed-in user's dashboard layout with the whole list sent
/// (always the full list, never a change), in screen order; a widget listed
/// twice keeps its first place. Each widget added or removed compared with the
/// saved layout is recorded for analytics (not on the first save, which has
/// nothing to compare with). Returns the layout as saved.
#[utoipa::path(
    put,
    path = "/v1/me/dashboard/widgets",
    operation_id = "save_my_dashboard_layout",
    tag = "dashboard",
    request_body = DashboardLayout,
    responses(
        (status = 200, description = "The layout as saved", body = DashboardLayout),
        (status = 400, description = "Not a list of known widget ids", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn save_layout(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(body): JsonBody<DashboardLayout>,
) -> ApiResult<Json<DashboardLayout>> {
    let mut widget_ids = Vec::with_capacity(body.widget_ids.len());
    for widget in body.widget_ids {
        if !widget_ids.contains(&widget) {
            widget_ids.push(widget);
        }
    }
    replace_layout(&state.pool, user.id, user.company_id, &widget_ids).await?;
    Ok(Json(DashboardLayout { widget_ids }))
}

/// Record a dashboard widget use
///
/// Stores one use of a dashboard widget (a click, a changed value, an add or
/// remove) for product analytics. Returns no body.
#[utoipa::path(
    post,
    path = "/v1/me/dashboard/widget-events",
    operation_id = "record_dashboard_widget_event",
    tag = "dashboard",
    request_body = WidgetEvent,
    responses(
        (status = 204, description = "Recorded"),
        (status = 400, description = "Unknown widget or action, or an empty or too long target", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn record_widget_event(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    JsonBody(event): JsonBody<WidgetEvent>,
) -> ApiResult<StatusCode> {
    let target = event.target.trim();
    if target.is_empty() || target.chars().count() > 64 {
        return Err(ApiError::BadRequest(
            "target must be 1 to 64 characters".into(),
        ));
    }
    record_event(
        &state.pool,
        user.id,
        user.company_id,
        event.widget_id,
        event.action,
        target,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
