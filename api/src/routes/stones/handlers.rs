use super::queries::list_stones;
use super::schemas::{ListStonesQuery, Stone};
use crate::auth::EmployeeUser;
use crate::error::{ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;

/// List the company's stones with slab availability
///
/// Returns the stones of the caller's company that are on display, sorted by
/// name, each with counts of its slabs in the yard (`slabs`). Sold out stones
/// (no available slab and not regular stock) are hidden unless
/// `include_sold_out=true`; hidden and deleted stones are never returned.
/// Filters combine with AND; list filters take the parameter repeated and
/// match any value. Color ids come from the colors list, supplier ids from
/// the suppliers list.
#[utoipa::path(
    get,
    path = "/v1/stones",
    operation_id = "list_stones",
    tag = "stones",
    params(ListStonesQuery),
    responses(
        (status = 200, description = "Stones sorted by name", body = Vec<Stone>),
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
    MultiQuery(query): MultiQuery<ListStonesQuery>,
) -> ApiResult<Json<Vec<Stone>>> {
    Ok(Json(
        list_stones(&state.pool, user.company_id, &query).await?,
    ))
}
