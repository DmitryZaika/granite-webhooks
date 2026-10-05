use super::queries::{UserNameRow, sales_reps_in_company};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the company's sales reps
///
/// Returns the non-deleted users of the caller's company who hold the sales
/// rep position. Use their ids for the `sales_rep` filter of `GET /v1/customers`.
#[utoipa::path(
    get,
    path = "/v1/users/sales-reps",
    operation_id = "list_sales_reps",
    tag = "users",
    responses(
        (status = 200, description = "Sales reps of the current company", body = Vec<UserNameRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn sales_reps(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<UserNameRow>>> {
    let rows = sales_reps_in_company(&state.pool, user.company_id).await?;
    Ok(Json(rows))
}
