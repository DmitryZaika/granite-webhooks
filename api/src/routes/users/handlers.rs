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

/// List the company's employees
///
/// The non-deleted employees of the caller's company by name ("User {id}"
/// when blank), e.g. to hide a calendar from one of them.
#[utoipa::path(
    get,
    path = "/v1/users/employees",
    operation_id = "list_employees",
    tag = "users",
    responses(
        (status = 200, description = "Employees of the current company", body = Vec<UserNameRow>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn employees(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
) -> ApiResult<Json<Vec<UserNameRow>>> {
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT id, name FROM users
          WHERE company_id = ? AND is_deleted = 0 AND is_employee = 1
          ORDER BY name, id",
    )
    .bind(user.company_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, name)| UserNameRow {
                id,
                name: Some(
                    name.map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| format!("User {id}")),
                ),
            })
            .collect(),
    ))
}
