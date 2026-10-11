use super::queries::list_checklists;
use super::schemas::Checklist;
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the company's post-installation checklists
///
/// Returns every checklist of the caller's company, newest first (ties by
/// id, highest first), with the installer's name and the result of each
/// check. The customer's signature is not included; the signed PDF of a
/// checklist is at `/api/checklist-pdf/{id}` on the web app.
#[utoipa::path(
    get,
    path = "/v1/checklists",
    operation_id = "list_checklists",
    tag = "checklists",
    responses(
        (status = 200, description = "Checklists, newest first", body = Vec<Checklist>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<Checklist>>> {
    Ok(Json(list_checklists(&state.pool, user.company_id).await?))
}
