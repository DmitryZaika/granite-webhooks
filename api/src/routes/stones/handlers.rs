use super::queries::{find_stone, list_stone_slabs, list_stones};
use super::schemas::{ListStonesQuery, Stone, StoneDetail, StoneSlab};
use crate::auth::EmployeeUser;
use crate::error::{ApiError, ApiResult, ErrorBody};
use crate::extract::MultiQuery;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, State};

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

/// Get one stone
///
/// Returns every field of a stone of the caller's company, including hidden
/// (not displayed) stones, plus the ids of its colors. Deleted stones and
/// stones of other companies are 404. List its slabs with
/// `GET /v1/stones/{id}/slabs`.
#[utoipa::path(
    get,
    path = "/v1/stones/{id}",
    operation_id = "get_stone",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "The stone", body = StoneDetail),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn get(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<StoneDetail>> {
    find_stone(&state.pool, user.company_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// List a stone's slabs
///
/// Returns the slabs and remnants in the yard for a stone: its own plus
/// those of the stones it draws from (linked stones, marked with
/// `source_stone`). Cut and deleted slabs are left out. Sold slabs carry
/// their sale (customer, seller, date, notes, room, sinks); remnants carry
/// the sale of the slab they were cut from. Ordered by slab id; group by
/// `bundle` for display. Sell an available slab with the sales routes,
/// resize one with `PATCH /v1/slabs/{id}`.
#[utoipa::path(
    get,
    path = "/v1/stones/{id}/slabs",
    operation_id = "list_stone_slabs",
    tag = "stones",
    params(("id" = i32, Path, description = "Stone id")),
    responses(
        (status = 200, description = "Slabs by id", body = Vec<StoneSlab>),
        (status = 401, description = "No valid session", body = ErrorBody),
        (status = 403, description = "Caller is not an employee or admin", body = ErrorBody),
        (status = 404, description = "No such stone in the caller's company", body = ErrorBody),
        (status = 500, description = "Database error", body = ErrorBody),
    ),
    security(("session_cookie" = []))
)]
pub async fn slabs(
    State(state): State<AppState>,
    EmployeeUser(user): EmployeeUser,
    Path(id): Path<i32>,
) -> ApiResult<Json<Vec<StoneSlab>>> {
    if find_stone(&state.pool, user.company_id, id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Ok(Json(
        list_stone_slabs(&state.pool, user.company_id, id).await?,
    ))
}
