use super::queries::list_faucets;
use super::schemas::{Faucet, ListFaucetsQuery};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the company's faucet models with free units
///
/// Returns the non-deleted faucet models of the caller's company, sorted by
/// name, each with the number of units free to sell (`available`). Models
/// not on display are included (`is_display`). Sold out models (no free unit
/// and not regular stock) are hidden unless `include_sold_out=true`. Filters
/// combine with AND; `type` takes the parameter repeated and matches any value.
#[utoipa::path(
    get,
    path = "/v1/faucets",
    operation_id = "list_faucets",
    tag = "faucets",
    params(ListFaucetsQuery),
    responses(
        (status = 200, description = "Faucet models sorted by name", body = Vec<Faucet>),
        (status = 400, description = "A filter has the wrong type, e.g. `supplier_id=abc`", body = ErrorBody),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    MultiQuery(query): MultiQuery<ListFaucetsQuery>,
) -> ApiResult<Json<Vec<Faucet>>> {
    Ok(Json(
        list_faucets(&state.pool, user.company_id, &query).await?,
    ))
}
